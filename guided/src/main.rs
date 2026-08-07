//! kazeta-guided — the guide menu daemon, for game sessions AND the dashboard.
//!
//! Owns the Guide button via InputPlumber's intercept modes, entirely over
//! D-Bus — the daemon never opens an input device, which is what keeps
//! controllers from rumbling (gilrs opens pads read-write) and keeps
//! RetroArch's own Guide binding from double-firing. Because mode 1 swallows
//! Guide before the virtual pad, and the passive X grab swallows keyboard
//! Home before the focused app, the bios's built-in guide modal simply never
//! triggers while this daemon runs — no bios change needed.
//!
//!   IDLE:  InterceptMode 1 — the app gets everything except Guide.
//!   OPEN:  InterceptMode 2 — ALL pad input arrives as D-Bus signals.
//!
//! gamescope rules learned the hard way:
//!  - GAMESCOPE_EXTERNAL_OVERLAY must be set BEFORE the first map, or the
//!    window counts as a second app and kills the game's swapchain.
//!  - Unmapping latches the last composited frame on screen. The window
//!    stays mapped once shown; "hidden" means cleared to transparent.
//!  - The compositor's process name is `gamescope-wl` (NOT `gamescope`).
//!
//! InputPlumber rule learned the hard way: never switch intercept mode while
//! the Guide button is still held — the release lands in the new mode with no
//! matching press and IP's guide latch eats the NEXT press. When Guide itself
//! closes the menu, the switch back to mode 1 waits for the release event.

use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::wrapper::ConnectionExt as _;

const SLATE: (u8, u8, u8) = (42, 44, 46);
const SLATE_ROW: (u8, u8, u8) = (58, 61, 64);
const GREEN: (u8, u8, u8) = (16, 124, 16);
const RED: (u8, u8, u8) = (140, 30, 30);
// Disabled rows keep their identity color, just darkened — grey reads as a
// different widget, a dim green/red reads as "this action, unavailable".
const GREEN_DIM: (u8, u8, u8) = (20, 56, 22);
const RED_DIM: (u8, u8, u8) = (58, 24, 24);

/// One row of the main guide menu. The dashboard and in-game menus carry
/// different row sets, so rows are identities, not indices.
#[derive(PartialEq, Clone, Copy)]
enum RowKind {
    Launch,       // dashboard: the cart launcher
    ReturnToGame, // in-game: close the guide
    KazetaHome,
    PowerOff,
}

const KS_HOME: u32 = 0xff50;
const KS_UP: u32 = 0xff52;
const KS_DOWN: u32 = 0xff54;
const KS_RETURN: u32 = 0xff0d;
const KS_ESCAPE: u32 = 0xff1b;
const KS_BACKSPACE: u32 = 0xff08;
const KS_E: u32 = 0x0065; // eject, matching the dashboard's keyboard binding

#[derive(PartialEq, Clone, Copy)]
enum Source {
    Pad,
    Keyboard,
}

// Button glyphs straight from the dashboard's legend set, picked by pad brand.
const GLYPH_XBOX_A: &[u8] = include_bytes!("../../bios/buttons/xbox_button_color_a.png");
const GLYPH_XBOX_B: &[u8] = include_bytes!("../../bios/buttons/xbox_button_color_b.png");
const GLYPH_PS_CROSS: &[u8] = include_bytes!("../../bios/buttons/playstation_button_color_cross.png");
const GLYPH_PS_CIRCLE: &[u8] = include_bytes!("../../bios/buttons/playstation_button_color_circle.png");
const GLYPH_SW_A: &[u8] = include_bytes!("../../bios/buttons/switch1_button_a.png");
const GLYPH_SW_B: &[u8] = include_bytes!("../../bios/buttons/switch1_button_b.png");
const GLYPH_KB_ENTER: &[u8] = include_bytes!("../../bios/buttons/keyboard_enter.png");
const GLYPH_KB_BACK: &[u8] = include_bytes!("../../bios/buttons/keyboard_backspace.png");
// North-face glyphs for the eject legend, same set the bios eject hint uses.
const GLYPH_XBOX_Y: &[u8] = include_bytes!("../../bios/buttons/xbox_button_color_y.png");
const GLYPH_PS_TRIANGLE: &[u8] = include_bytes!("../../bios/buttons/playstation_button_color_triangle.png");
const GLYPH_SW_X: &[u8] = include_bytes!("../../bios/buttons/switch1_button_x.png");
const GLYPH_KB_E: &[u8] = include_bytes!("../../bios/buttons/keyboard_e.png");
const ICON_CONTROLLER: &[u8] = include_bytes!("../../bios/CONTROLLER.png");
const ICON_SDCARD: &[u8] = include_bytes!("../../bios/SDCARD.png");

/// Player LED colors, same order the pad LED service assigns them.
const PLAYER_COLORS: [(u8, u8, u8); 4] =
    [(40, 180, 40), (50, 110, 220), (200, 50, 50), (220, 190, 40)];

/// Battery of the pad behind composite device `idx` — (percent, charging).
/// Controller batteries are scope=Device supplies; sorted for a stable order.
fn read_battery(idx: usize) -> Option<(u8, bool)> {
    let mut found: Vec<(String, u8, bool)> = Vec::new();
    for e in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let scope = std::fs::read_to_string(p.join("scope")).unwrap_or_default();
        if scope.trim() != "Device" && !name.contains("controller") {
            continue;
        }
        let Some(cap) = std::fs::read_to_string(p.join("capacity"))
            .ok()
            .and_then(|c| c.trim().parse::<u8>().ok())
        else {
            continue;
        };
        let charging = std::fs::read_to_string(p.join("status"))
            .map(|s| s.trim() == "Charging")
            .unwrap_or(false);
        found.push((name, cap, charging));
    }
    found.sort();
    found
        .get(idx)
        .or_else(|| found.first())
        .map(|(_, c, ch)| (*c, *ch))
}

fn log(msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/kazeta-guided.log")
    {
        let _ = writeln!(f, "{}", msg);
    }
}

fn set_intercept(mode: u32) {
    for i in 0..4 {
        let _ = Command::new("busctl")
            .args([
                "set-property",
                "org.shadowblip.InputPlumber",
                &format!("/org/shadowblip/InputPlumber/CompositeDevice{}", i),
                "org.shadowblip.Input.CompositeDevice",
                "InterceptMode",
                "u",
                &mode.to_string(),
            ])
            .stderr(Stdio::null())
            .status();
    }
    log(&format!("intercept -> {}", mode));
}

fn pad_brand() -> String {
    Command::new("busctl")
        .args([
            "get-property",
            "org.shadowblip.InputPlumber",
            "/org/shadowblip/InputPlumber/CompositeDevice0",
            "org.shadowblip.Input.CompositeDevice",
            "Name",
        ])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

/// Pure LED colors for the lightbar itself (the UI's PLAYER_COLORS are
/// display-tuned pastels). Same order as the bios's pad_leds painter.
const LED_COLORS: [(u8, u8, u8); 4] = [(0, 255, 0), (0, 0, 255), (255, 0, 0), (255, 255, 0)];

/// Physical pads in join order — a lean port of the bios's pad_leds scan.
/// Skips InputPlumber's virtual mirrors so one controller is one slot.
fn scan_physical_pads() -> Vec<u32> {
    const PAD_WORDS: [&str; 7] =
        ["controller", "gamepad", "8bitdo", "joystick", "joy-con", "xbox", "x-box"];
    const NOT_PAD_WORDS: [&str; 5] = ["motion", "imu", "touchpad", "keyboard", "mouse"];
    let mut pads = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/input") {
        for entry in entries.flatten() {
            let dir = entry.file_name().to_string_lossy().into_owned();
            let Some(num) = dir.strip_prefix("input").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let name = std::fs::read_to_string(entry.path().join("name"))
                .unwrap_or_default()
                .trim()
                .to_lowercase();
            if !PAD_WORDS.iter().any(|w| name.contains(w))
                || NOT_PAD_WORDS.iter().any(|w| name.contains(w))
            {
                continue;
            }
            let virtual_dev = std::fs::canonicalize(entry.path())
                .map(|p| {
                    let p = p.to_string_lossy().into_owned();
                    p.contains("uhid") || p.contains("/virtual/")
                })
                .unwrap_or(true);
            if virtual_dev {
                continue;
            }
            pads.push(num);
        }
    }
    pads.sort_unstable();
    pads
}

/// One-shot player-color paint (lightbar + white player dot). The bios's
/// painter re-asserts continuously on the dashboard but dies with the bios at
/// game launch — in-game, a reconnected pad would otherwise stay stock blue.
/// One shot on arrival restores the color without fighting a game that later
/// sets its own.
fn paint_pad_leds() {
    for (slot, num) in scan_physical_pads().into_iter().take(4).enumerate() {
        let base = format!("/sys/class/leds/input{}:rgb:indicator", num);
        if std::fs::metadata(&base).is_err() {
            continue;
        }
        let (r, g, b) = LED_COLORS[slot];
        let _ = std::fs::write(format!("{}/multi_intensity", base), format!("{} {} {}", r, g, b));
        let _ = std::fs::write(format!("{}/brightness", base), "255");
        for dot in 1..=5 {
            let dot_path = format!(
                "/sys/class/leds/input{}:white:player-{}/brightness",
                num, dot
            );
            let _ = std::fs::write(dot_path, if dot == slot + 1 { "1" } else { "0" });
        }
    }
}

/// Indices of the composite devices InputPlumber currently manages — one per
/// physical pad, in join order.
fn list_composites() -> Vec<usize> {
    let out = Command::new("busctl")
        .args(["tree", "org.shadowblip.InputPlumber"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut ids: Vec<usize> = out
        .lines()
        .filter_map(|l| {
            let p = l.find("CompositeDevice")?;
            l[p + 15..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn dashboard_running() -> bool {
    Command::new("pgrep")
        .args(["-x", "kazeta-bios"])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn kill_game() {
    // The compositor's comm is gamescope-wl; -f on the exec file is the
    // belt-and-braces for anything the name check misses.
    let a = Command::new("pkill").args(["-x", "gamescope-wl"]).status();
    let b = Command::new("pkill").args(["-f", "kazeta-cart-exec"]).status();
    log(&format!("kill_game: gamescope-wl={:?} cart-exec={:?}", a, b));
}

struct CartGame {
    name: String,
    kzi: PathBuf,
    icon: Option<Rgba>,
}

struct Cart {
    name: String,
    icon: Option<Rgba>,
    games: Vec<CartGame>,
}

/// The inserted cart, for the dashboard's launch row: every .kzi on mounted
/// media, plus the collection identity from cartinfo.yaml when present.
fn scan_cart() -> Option<Cart> {
    let media = std::fs::read_dir("/run/media").ok()?;
    for m in media.flatten() {
        let dir = m.path();
        let Ok(files) = std::fs::read_dir(&dir) else { continue };
        let mut games = Vec::new();
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map(|e| e == "kzi").unwrap_or(false) {
                let Ok(content) = std::fs::read_to_string(&p) else { continue };
                let mut name = None;
                let mut icon = None;
                for line in content.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let v = v.trim().trim_matches('"');
                        match k.trim() {
                            "Name" => name = Some(v.to_string()),
                            "Icon" => icon = Some(v.to_string()),
                            _ => {}
                        }
                    }
                }
                let icon = icon
                    .map(|i| dir.join(i))
                    .and_then(|p| std::fs::read(p).ok())
                    .and_then(|b| decode_png(&b));
                games.push(CartGame {
                    name: name.unwrap_or_else(|| {
                        p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                    }),
                    kzi: p,
                    icon,
                });
            }
        }
        if games.is_empty() {
            continue;
        }
        games.sort_by(|a, b| a.name.cmp(&b.name));

        // Collection identity: cartinfo.yaml wins, then a lone game's own.
        let mut cart_name = None;
        let mut cart_icon_path = None;
        if let Ok(yaml) = std::fs::read_to_string(dir.join("cartinfo.yaml")) {
            for line in yaml.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    let v = v.trim().trim_matches('"').trim_matches('\'');
                    match k.trim() {
                        "name" if !v.is_empty() => cart_name = Some(v.to_string()),
                        "icon" if !v.is_empty() => cart_icon_path = Some(dir.join(v)),
                        _ => {}
                    }
                }
            }
        }
        let cart_icon = cart_icon_path
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| decode_png(&b))
            .or_else(|| if games.len() == 1 { games[0].icon.clone() } else { None });
        let name = cart_name.unwrap_or_else(|| {
            if games.len() == 1 {
                games[0].name.clone()
            } else {
                format!("Multi-Cart ({} games)", games.len())
            }
        });
        return Some(Cart { name, icon: cart_icon, games });
    }
    None
}

/// Launch a kzi from the dashboard, riding the bios's own machinery: stage
/// the launch command plus the restart sentinel (without it, kazeta-session
/// interprets the bios exiting as a shutdown request), then end the bios.
fn launch_kzi(kzi: &std::path::Path) {
    let _ = std::fs::write(
        "/var/kazeta/state/.LAUNCH_CMD",
        format!("/usr/bin/kazeta '{}'\n", kzi.display()),
    );
    let _ = std::fs::write("/var/kazeta/state/.RESTART_SESSION_SENTINEL", "");
    let r = Command::new("pkill").args(["-x", "kazeta-bios"]).status();
    log(&format!("launch {} pkill bios={:?}", kzi.display(), r));
}

/// Name + icon of the running game, resolved by matching the exec line the
/// kazeta script staged against the .kzi files on mounted media.
fn resolve_game() -> Option<(String, Option<PathBuf>)> {
    let exec = std::fs::read_to_string("/tmp/kazeta-cart-exec").ok()?;
    let exec = exec.trim().trim_matches('"').to_string();
    if exec.is_empty() {
        return None;
    }
    let media = std::fs::read_dir("/run/media").ok()?;
    for m in media.flatten() {
        let Ok(files) = std::fs::read_dir(m.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().map(|e| e == "kzi").unwrap_or(false) {
                let Ok(content) = std::fs::read_to_string(&p) else { continue };
                let mut name = None;
                let mut icon = None;
                let mut kzi_exec = None;
                for line in content.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        let v = v.trim().trim_matches('"');
                        match k.trim() {
                            "Name" => name = Some(v.to_string()),
                            "Icon" => icon = Some(v.to_string()),
                            "Exec" => kzi_exec = Some(v.to_string()),
                            _ => {}
                        }
                    }
                }
                if kzi_exec.as_deref() == Some(exec.as_str()) {
                    let dir = p.parent().unwrap_or(std::path::Path::new("/"));
                    return Some((
                        name.unwrap_or_else(|| "Kazeta+".into()),
                        icon.map(|i| dir.join(i)),
                    ));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------- sounds ---

struct Sfx {
    player: Option<&'static str>,
    dir: Option<PathBuf>,
    volume: f32,
}

impl Sfx {
    fn find() -> Self {
        let player = ["pw-play", "paplay", "aplay"].into_iter().find(|p| {
            Command::new("which")
                .arg(p)
                .stdout(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });
        let dir = [
            "/home/gamer/.local/share/kazeta-plus/themes/Metro360/Metro360SFX",
            "/usr/share/kazeta-plus/themes/Metro360/Metro360SFX",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|d| d.join("move.wav").exists());
        let volume = std::fs::read_to_string("/home/gamer/.local/share/kazeta-plus/config.toml")
            .ok()
            .and_then(|c| {
                c.lines()
                    .find(|l| l.trim_start().starts_with("sfx_volume"))
                    .and_then(|l| l.split('=').nth(1))
                    .and_then(|v| v.trim().parse::<f32>().ok())
            })
            .unwrap_or(0.7);
        log(&format!("sfx: player={:?} dir={:?} vol={}", player, dir, volume));
        Self { player, dir, volume }
    }

    fn play(&self, name: &str) {
        let (Some(player), Some(dir)) = (self.player, &self.dir) else { return };
        let path = dir.join(name);
        if !path.exists() {
            return;
        }
        let mut cmd = Command::new(player);
        if player == "pw-play" {
            cmd.arg(format!("--volume={}", self.volume));
        }
        let _ = cmd.arg(path).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}

// ---------------------------------------------------------------- canvas ---

#[derive(Clone)]
struct Rgba {
    w: usize,
    h: usize,
    px: Vec<u8>, // straight-alpha RGBA
}

fn decode_png(bytes: &[u8]) -> Option<Rgba> {
    let mut decoder = png::Decoder::new(bytes);
    // The dashboard button glyphs are palette PNGs (color type 3) with tRNS
    // transparency — expand everything to plain 8-bit channels.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px = match info.color_type {
        png::ColorType::Rgba => buf[..w * h * 4].to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(w * h * 4);
            for c in buf[..w * h * 3].chunks(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(w * h * 4);
            for c in buf[..w * h * 2].chunks(2) {
                out.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(w * h * 4);
            for &g in &buf[..w * h] {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        _ => return None,
    };
    Some(Rgba { w, h, px })
}

/// Premultiplied-ARGB software canvas. gamescope blends premultiplied.
#[derive(Clone)]
struct Canvas {
    w: usize,
    h: usize,
    px: Vec<u32>,
}

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, px: vec![0; w * h] }
    }
    /// Alpha-blend a rect over existing pixels (fill() replaces instead).
    fn blend_fill(&mut self, x: i32, y: i32, w: i32, h: i32, rgb: (u8, u8, u8), a: u8) {
        let al = a as u32;
        let pm = |c: u8| c as u32 * al / 255;
        let (r, g, b) = (pm(rgb.0), pm(rgb.1), pm(rgb.2));
        for yy in y.max(0)..(y + h).min(self.h as i32) {
            for xx in x.max(0)..(x + w).min(self.w as i32) {
                self.over(xx, yy, al, r, g, b);
            }
        }
    }
    fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, rgb: (u8, u8, u8), a: u8) {
        let al = a as u32;
        let pm = |c: u8| c as u32 * al / 255;
        let v = (al << 24) | (pm(rgb.0) << 16) | (pm(rgb.1) << 8) | pm(rgb.2);
        for yy in y.max(0)..(y + h).min(self.h as i32) {
            let row = yy as usize * self.w;
            for xx in x.max(0)..(x + w).min(self.w as i32) {
                self.px[row + xx as usize] = v;
            }
        }
    }
    fn over(&mut self, x: i32, y: i32, a: u32, r: u32, g: u32, b: u32) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 || a == 0 {
            return;
        }
        let i = y as usize * self.w + x as usize;
        let dst = self.px[i];
        let inv = 255 - a;
        let bl = |shift: u32, src: u32| {
            let d = (dst >> shift) & 0xff;
            ((src + d * inv / 255).min(255)) << shift
        };
        self.px[i] = bl(24, a) | bl(16, r) | bl(8, g) | bl(0, b);
    }
    /// Nearest-scaled straight-alpha RGBA sprite, alpha-blended over.
    fn blit(&mut self, img: &Rgba, x: i32, y: i32, size: i32) {
        if img.w == 0 || img.h == 0 || size <= 0 {
            return;
        }
        for dy in 0..size {
            let sy = (dy as usize * img.h) / size as usize;
            for dx in 0..size {
                let sx = (dx as usize * img.w) / size as usize;
                let i = (sy * img.w + sx) * 4;
                let a = img.px[i + 3] as u32;
                let pm = |c: u8| c as u32 * a / 255;
                self.over(x + dx, y + dy, a, pm(img.px[i]), pm(img.px[i + 1]), pm(img.px[i + 2]));
            }
        }
    }
    fn glyph(&mut self, x: i32, y: i32, gw: usize, gh: usize, cov: &[u8], alpha: u8) {
        for gy in 0..gh {
            for gx in 0..gw {
                let c = (cov[gy * gw + gx] as u32 * alpha as u32) / 255;
                if c > 0 {
                    self.over(x + gx as i32, y + gy as i32, c, c, c, c);
                }
            }
        }
    }
    fn text(&mut self, font: &fontdue::Font, s: &str, x: i32, y: i32, size: f32, alpha: u8) {
        let mut pen = x as f32;
        for ch in s.chars() {
            let (m, cov) = font.rasterize(ch, size);
            self.glyph(pen as i32 + m.xmin, y - m.height as i32 - m.ymin, m.width, m.height, &cov, alpha);
            pen += m.advance_width;
        }
    }
    fn text_width(&self, font: &fontdue::Font, s: &str, size: f32) -> i32 {
        s.chars().map(|c| font.metrics(c, size).advance_width).sum::<f32>() as i32
    }
    fn frame_scaled(&self, k: f32, alpha: f32) -> Canvas {
        let mut out = Canvas::new(self.w, self.h);
        let kw = (self.w as f32 * k) as i32;
        let kh = (self.h as f32 * k) as i32;
        let ox = (self.w as i32 - kw) / 2;
        let oy = (self.h as i32 - kh) / 2;
        let a = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
        for dy in 0..kh {
            let sy = (dy as f32 / k) as usize;
            if sy >= self.h {
                continue;
            }
            let srow = sy * self.w;
            let drow = (dy + oy) as usize * out.w;
            for dx in 0..kw {
                let sx = (dx as f32 / k) as usize;
                if sx >= self.w {
                    continue;
                }
                let s = self.px[srow + sx];
                let mul = |shift: u32| (((s >> shift) & 0xff) * a / 255) << shift;
                out.px[drow + (dx + ox) as usize] = mul(24) | mul(16) | mul(8) | mul(0);
            }
        }
        out
    }
    fn to_le_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.px.len() * 4);
        for v in &self.px {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes
    }
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// --------------------------------------------------------------- overlay ---

struct Overlay {
    conn: x11rb::rust_connection::RustConnection,
    win: Window,
    root: Window,
    gc: Gcontext,
    sw: u16,
    sh: u16,
    pw: usize,
    ph: usize,
    px: i16,
    py: i16,
    mapped: bool,
    keymap: Vec<(u8, u32)>,
    strip_top: Vec<u8>,
    strip_bot: Vec<u8>,
    top_h: u16,
    bot_h: u16,
    /// No compositor (the VM: startx, no gamescope). Bare X ignores alpha —
    /// "transparent" renders as solid black — so the dim is faked instead:
    /// snapshot the screen at open, darken it in software, paint that as the
    /// backdrop, and unmap on close.
    bare_x: bool,
    /// The darkened screen snapshot (0xFFRRGGBB per pixel), bare X only.
    snap: Vec<u32>,
}

/// Shadow border width around the sheet, in 1080p design units.
const PANEL_PAD_DU: f32 = 26.0;
/// Downward shadow offset. The canvas gets this much extra bottom padding so
/// the shifted falloff fades out INSIDE the canvas instead of clipping at its
/// edge (the clip read as a seam under the panel).
const PANEL_DROP_DU: f32 = 5.0;

impl Overlay {
    fn connect() -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let (sw, sh) = (screen.width_in_pixels, screen.height_in_pixels);

        let depth = screen
            .allowed_depths
            .iter()
            .find(|d| d.depth == 32)
            .ok_or("no 32-bit depth")?;
        let visual = depth.visuals.first().ok_or("no 32-bit visual")?.visual_id;

        let colormap = conn.generate_id()?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, root, visual)?;

        // Gamescope acts as the window manager and stamps the EWMH check
        // window; a session with no WM at all is the VM's bare X server.
        let wm_check = conn.intern_atom(true, b"_NET_SUPPORTING_WM_CHECK")?.reply()?.atom;
        let bare_x = wm_check == x11rb::NONE
            || conn
                .get_property(false, root, wm_check, AtomEnum::WINDOW, 0, 1)?
                .reply()
                .map(|r| r.value_len == 0)
                .unwrap_or(true);
        log(&format!("compositor: {}", if bare_x { "none (windowed mode)" } else { "gamescope" }));

        let s = sh as f32 / 1080.0;
        // Canvas carries a pad border around the sheet for the drop shadow.
        let pad = (PANEL_PAD_DU * s) as usize;
        let pw = (560.0 * s) as usize + pad * 2;
        let ph = (370.0 * s) as usize + pad * 2 + (PANEL_DROP_DU * s) as usize;
        let px = ((sw as usize - pw) / 2) as i16;
        let py = ((sh as usize - ph) / 2) as i16;

        let win = conn.generate_id()?;
        conn.create_window(
            32,
            win,
            root,
            0,
            0,
            sw,
            sh,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new()
                .background_pixel(0)
                .border_pixel(0)
                .override_redirect(1)
                .colormap(colormap)
                .event_mask(EventMask::EXPOSURE),
        )?;

        let atom = conn
            .intern_atom(false, b"GAMESCOPE_EXTERNAL_OVERLAY")?
            .reply()?
            .atom;
        conn.change_property32(PropMode::REPLACE, win, atom, AtomEnum::CARDINAL, &[1])?;
        conn.sync()?;

        let gc = conn.generate_id()?;
        conn.create_gc(gc, win, &CreateGCAux::new())?;

        // Screen-edge fade strips: the dim behind the panel deepens toward
        // the top and bottom edges, the way Metro screens frame themselves.
        // Core X can't gradient-fill, so bake premultiplied-black strips once.
        let top_h = (90.0 * s) as u16;
        let bot_h = (64.0 * s) as u16;
        let mut bake = |hgt: u16, flip: bool| -> Vec<u8> {
            let mut bytes = Vec::with_capacity(sw as usize * hgt as usize * 4);
            for y in 0..hgt {
                let t = y as f32 / hgt as f32;
                let t = if flip { t } else { 1.0 - t };
                let a = ((0.55 + 0.30 * t.powf(1.5)) * 255.0) as u8;
                let v = ((a as u32) << 24).to_le_bytes();
                for _ in 0..sw {
                    bytes.extend_from_slice(&v);
                }
            }
            bytes
        };
        let strip_top = bake(top_h, false);
        let strip_bot = bake(bot_h, true);

        let setup = conn.setup();
        let (min_kc, max_kc) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min_kc, max_kc - min_kc + 1)?.reply()?;
        let per = reply.keysyms_per_keycode as usize;
        let mut keymap = Vec::new();
        for (i, chunk) in reply.keysyms.chunks(per).enumerate() {
            if let Some(&ks) = chunk.iter().find(|&&k| k != 0) {
                keymap.push((min_kc + i as u8, ks));
            }
        }

        Ok(Self {
            conn, win, root, gc, sw, sh, pw, ph, px, py,
            mapped: false, keymap, strip_top, strip_bot, top_h, bot_h, bare_x,
            snap: Vec::new(),
        })
    }

    /// The session script may start us before gamescope's Xwayland is up.
    fn connect_with_retry() -> Option<Self> {
        for _ in 0..60 {
            match Self::connect() {
                Ok(o) => return Some(o),
                Err(_) => std::thread::sleep(Duration::from_millis(500)),
            }
        }
        None
    }

    fn keycode_for(&self, keysym: u32) -> Option<u8> {
        self.keymap.iter().find(|(_, ks)| *ks == keysym).map(|(kc, _)| *kc)
    }
    fn keysym_for(&self, keycode: u8) -> u32 {
        self.keymap
            .iter()
            .find(|(kc, _)| *kc == keycode)
            .map(|(_, ks)| *ks)
            .unwrap_or(0)
    }

    fn grab_home(&self) {
        if let Some(kc) = self.keycode_for(KS_HOME) {
            let ok = self
                .conn
                .grab_key(false, self.root, ModMask::ANY, kc, GrabMode::ASYNC, GrabMode::ASYNC)
                .is_ok();
            log(&format!("grab Home kc={} ok={}", kc, ok));
            let _ = self.conn.flush();
        }
    }
    fn grab_keyboard(&self) {
        let _ = self
            .conn
            .grab_keyboard(false, self.root, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC);
        let _ = self.conn.flush();
    }
    fn ungrab_keyboard(&self) {
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.flush();
    }

    fn ensure_mapped(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.bare_x {
            // Mapping happens inside backdrop(), after the screen capture —
            // capturing a mapped overlay would photograph itself.
            return Ok(());
        }
        if !self.mapped {
            self.conn.map_window(self.win)?;
            self.conn.flush()?;
            self.mapped = true;
        }
        Ok(())
    }

    /// Bare X: photograph the root window and darken it, edge strips baked
    /// in, so the guide gets its dim even with no compositor to blend one.
    /// The background freezes while the guide is open — the honest trade.
    fn capture_dim(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.snap.clear();
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, self.sw, self.sh, !0u32)?
            .reply()?;
        let w = self.sw as usize;
        let n = w * self.sh as usize;
        if img.data.len() < n * 4 {
            // Odd root depth; put_panel falls back to the flat-dim composite.
            return Ok(());
        }
        let mut snap = vec![0u32; n];
        for i in 0..n {
            let b = (img.data[i * 4] as u32) * 45 / 100;
            let g = (img.data[i * 4 + 1] as u32) * 45 / 100;
            let r = (img.data[i * 4 + 2] as u32) * 45 / 100;
            snap[i] = 0xff00_0000 | (r << 16) | (g << 8) | b;
        }
        // Edge strips, same curve the gamescope backdrop bakes.
        let mut strip = |snap: &mut Vec<u32>, rows: usize, flip: bool| {
            for y in 0..rows {
                let t = y as f32 / rows.max(1) as f32;
                let t = if flip { t } else { 1.0 - t };
                let keep = 1.0 - (0.55 + 0.30 * t.powf(1.5));
                let row = if flip { self.sh as usize - rows + y } else { y };
                for x in 0..w {
                    let i = row * w + x;
                    let b = ((img.data[i * 4] as f32) * keep) as u32;
                    let g = ((img.data[i * 4 + 1] as f32) * keep) as u32;
                    let r = ((img.data[i * 4 + 2] as f32) * keep) as u32;
                    snap[i] = 0xff00_0000 | (r << 16) | (g << 8) | b;
                }
            }
        };
        strip(&mut snap, self.top_h as usize, false);
        strip(&mut snap, self.bot_h as usize, true);
        self.snap = snap;
        Ok(())
    }
    /// Flat dim plus the baked edge gradients.
    fn backdrop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.bare_x {
            if !self.mapped {
                let _ = self.capture_dim();
                self.conn.map_window(self.win)?;
                self.conn.flush()?;
                self.mapped = true;
            }
            if !self.snap.is_empty() {
                let bytes: Vec<u8> = self.snap.iter().flat_map(|v| v.to_le_bytes()).collect();
                self.conn.put_image(
                    ImageFormat::Z_PIXMAP, self.win, self.gc,
                    self.sw, self.sh, 0, 0, 0, 32, &bytes,
                )?;
                self.conn.flush()?;
            }
            return Ok(());
        }
        self.conn.change_gc(self.gc, &ChangeGCAux::new().foreground(0x8C000000))?;
        self.conn.poly_fill_rectangle(
            self.win,
            self.gc,
            &[Rectangle { x: 0, y: 0, width: self.sw, height: self.sh }],
        )?;
        self.conn.put_image(
            ImageFormat::Z_PIXMAP, self.win, self.gc,
            self.sw, self.top_h, 0, 0, 0, 32, &self.strip_top,
        )?;
        self.conn.put_image(
            ImageFormat::Z_PIXMAP, self.win, self.gc,
            self.sw, self.bot_h, 0, (self.sh - self.bot_h) as i16, 0, 32, &self.strip_bot,
        )?;
        self.conn.flush()?;
        Ok(())
    }
    fn put_panel(&self, cv: &Canvas) -> Result<(), Box<dyn std::error::Error>> {
        // PutImage REPLACES pixels, so the canvas's transparent shadow border
        // would punch a lighter hole in the backdrop dim. Composite every
        // frame over the dim here so the panel region stays as dark as the
        // rest of the screen.
        const DIM_A: u32 = 0x8C;
        let use_snap = self.bare_x && self.snap.len() == self.sw as usize * self.sh as usize;
        let mut bytes = Vec::with_capacity(cv.px.len() * 4);
        if use_snap {
            // Bare X: composite the panel over the darkened snapshot region,
            // since there is no compositor to blend the shadow border for us.
            let w = self.sw as usize;
            for row in 0..self.ph {
                let base = (self.py as usize + row) * w + self.px as usize;
                for col in 0..self.pw {
                    let v = cv.px[row * self.pw + col];
                    let a = v >> 24;
                    let s = self.snap[base + col];
                    let inv = 255 - a;
                    let r = ((v >> 16) & 0xff) + ((s >> 16) & 0xff) * inv / 255;
                    let g = ((v >> 8) & 0xff) + ((s >> 8) & 0xff) * inv / 255;
                    let b = (v & 0xff) + (s & 0xff) * inv / 255;
                    bytes.extend_from_slice(
                        &(0xff00_0000 | (r << 16) | (g << 8) | b).to_le_bytes(),
                    );
                }
            }
        } else {
            for v in &cv.px {
                let a = v >> 24;
                let out_a = a + DIM_A * (255 - a) / 255;
                // Dim is black: premultiplied color channels gain nothing.
                let out = (out_a << 24) | (v & 0x00ff_ffff);
                bytes.extend_from_slice(&out.to_le_bytes());
            }
        }
        self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            self.win,
            self.gc,
            self.pw as u16,
            self.ph as u16,
            self.px,
            self.py,
            0,
            32,
            &bytes,
        )?;
        self.conn.flush()?;
        Ok(())
    }
    /// "Hide": clear to transparent. The window STAYS mapped — gamescope
    /// latches the last frame of an unmapped overlay on screen. (Bare X has
    /// no latch bug and a mapped window is opaque there, so it unmaps.)
    fn clear(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.bare_x {
            self.conn.unmap_window(self.win)?;
            self.conn.flush()?;
            self.mapped = false;
            return Ok(());
        }
        self.conn.clear_area(false, self.win, 0, 0, self.sw, self.sh)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Raw canvas upload at an arbitrary spot (toasts).
    fn put_at(&self, cv: &Canvas, x: i16, y: i16) -> Result<(), Box<dyn std::error::Error>> {
        if self.bare_x {
            // Toasts are in-game furniture; games never run on bare X.
            return Ok(());
        }
        self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            self.win,
            self.gc,
            cv.w as u16,
            cv.h as u16,
            x,
            y,
            0,
            32,
            &cv.to_le_bytes(),
        )?;
        self.conn.flush()?;
        Ok(())
    }

    fn clear_rect(&self, x: i16, y: i16, w: u16, h: u16) -> Result<(), Box<dyn std::error::Error>> {
        if self.bare_x {
            return Ok(());
        }
        self.conn.clear_area(false, self.win, x, y, w, h)?;
        self.conn.flush()?;
        Ok(())
    }
}

// ----------------------------------------------------------------- panel ---

#[derive(PartialEq, Clone, Copy)]
enum UiMode {
    Menu,
    Games,
}

struct Ui {
    font: fontdue::Font,
    pad_a: Option<Rgba>,
    pad_b: Option<Rgba>,
    pad_y: Option<Rgba>,
    kb_a: Option<Rgba>,
    kb_b: Option<Rgba>,
    kb_y: Option<Rgba>,
    controller_icon: Option<Rgba>,
    sd_icon: Option<Rgba>,
    source: Source,
    game: Option<(String, Option<Rgba>)>,
    dashboard: bool,
    /// Inserted cart (dashboard only) — drives the launch row.
    cart: Option<Cart>,
    mode: UiMode,
    game_sel: usize,
    /// (player index, battery) of the pad that opened the menu; None when the
    /// keyboard opened it.
    opener: Option<(usize, Option<(u8, bool)>)>,
}

impl Ui {
    /// The main menu's rows for the current context.
    fn rows(&self) -> &'static [RowKind] {
        if self.dashboard {
            &[RowKind::Launch, RowKind::KazetaHome, RowKind::PowerOff]
        } else {
            &[RowKind::ReturnToGame, RowKind::KazetaHome, RowKind::PowerOff]
        }
    }
    fn enabled(&self, kind: RowKind) -> bool {
        match kind {
            // The launcher needs a cart in the slot.
            RowKind::Launch => self.cart.is_some(),
            // Already home on the dashboard.
            RowKind::KazetaHome => !self.dashboard,
            _ => true,
        }
    }
    fn label(&self, kind: RowKind) -> &'static str {
        match kind {
            RowKind::Launch => "insert cartridge", // replaced by the cart's name
            RowKind::ReturnToGame => "return to game",
            RowKind::KazetaHome => "kazeta home",
            RowKind::PowerOff => "power off",
        }
    }

    /// Eject rides the legend, not the row list: it lights up only while the
    /// cursor rests on the launcher row with a cart actually inserted.
    fn eject_available(&self, sel: usize) -> bool {
        self.mode == UiMode::Menu
            && self.dashboard
            && self.cart.is_some()
            && self.rows().get(sel) == Some(&RowKind::Launch)
    }

    fn render(&self, pw: usize, ph: usize, sh: u16, sel: usize, flash: Option<usize>) -> Canvas {
        let s = sh as f32 / 1080.0;
        let pad = (PANEL_PAD_DU * s) as i32;
        let padf = pad as f32;
        let drop = (PANEL_DROP_DU * s) as i32;
        let sheet_w = pw as i32 - pad * 2;
        let sheet_h = ph as i32 - pad * 2 - drop;
        let mut cv = Canvas::new(pw, ph);

        // Drop shadow: soft dark falloff in the pad border, nudged downward
        // so the sheet reads as lifted, not outlined.
        let (sx0, sy0) = (pad, pad + drop);
        let (sx1, sy1) = (pad + sheet_w, pad + sheet_h + drop);
        // Interior pixels get FULL shadow (the opaque sheet overpaints its
        // own area next) — treating the interior as "skip" left a bare strip
        // between the sheet's bottom edge and the down-shifted shadow rect.
        for y in 0..ph as i32 {
            let dy = if y < sy0 { sy0 - y } else if y >= sy1 { y - sy1 + 1 } else { 0 };
            for x in 0..pw as i32 {
                let dx = if x < sx0 { sx0 - x } else if x >= sx1 { x - sx1 + 1 } else { 0 };
                let d = ((dx * dx + dy * dy) as f32).sqrt();
                if d >= padf {
                    continue;
                }
                let t = 1.0 - d / padf;
                let a = (0.55 * t * t * 255.0) as u32;
                cv.px[y as usize * pw + x as usize] = a << 24;
            }
        }

        // The sheet, inset past the shadow border.
        // Fully opaque: nothing of the game bleeds through the sheet itself;
        // only the shadow and backdrop around it stay translucent.
        let (ox, oy) = (pad, pad);
        cv.fill(ox, oy, sheet_w, sheet_h, SLATE, 255);
        cv.fill(ox, oy, (4.0 * s) as i32, sheet_h, GREEN, 255);
        // FADE_TEX idiom: white sheen bleeding down from the top edge, dark
        // ramp rising from the bottom — under the content, like the bios.
        let sheen_h = (sheet_h as f32 * 0.16) as i32;
        for i in 0..sheen_h {
            let a = (34.0 * (1.0 - i as f32 / sheen_h as f32)) as u8;
            cv.blend_fill(ox, oy + i, sheet_w, 1, (255, 255, 255), a);
        }
        let ramp_h = (sheet_h as f32 * 0.25) as i32;
        for i in 0..ramp_h {
            let a = (88.0 * (i as f32 / ramp_h as f32)) as u8;
            cv.blend_fill(ox, oy + sheet_h - ramp_h + i, sheet_w, 1, (0, 0, 0), a);
        }

        let margin = ox + (36.0 * s) as i32;
        // Right edge of the content area, inside the sheet.
        let inner_r = ox + sheet_w - (36.0 * s) as i32;

        // Header: the game list shows the cart's identity; otherwise the
        // running game's, or the console's own.
        let (title, icon) = if self.mode == UiMode::Games {
            match &self.cart {
                Some(c) => (c.name.as_str(), c.icon.as_ref()),
                None => ("Kazeta+", None),
            }
        } else {
            match &self.game {
                Some((name, icon)) => (name.as_str(), icon.as_ref()),
                None => ("Kazeta+", None),
            }
        };
        let mut tx = margin;
        if let Some(img) = icon {
            let isz = (40.0 * s) as i32;
            cv.blit(img, margin, oy + (28.0 * s) as i32, isz);
            tx += isz + (14.0 * s) as i32;
        }
        // Squeeze long multicart names into the panel.
        let mut tsize = 34.0 * s;
        let avail = inner_r - tx;
        while tsize > 16.0 * s && cv.text_width(&self.font, title, tsize) > avail {
            tsize *= 0.92;
        }
        cv.text(&self.font, title, tx, oy + (62.0 * s) as i32, tsize, 255);
        cv.blend_fill(margin, oy + (84.0 * s) as i32, inner_r - margin, 1.max((1.0 * s) as i32), (255, 255, 255), 30);

        let rw = inner_r - margin;
        if self.mode == UiMode::Menu {
            let rows = self.rows();
            // Four dashboard rows sit tighter than the in-game three so the
            // legend row keeps its clearance.
            let (row_h, row_gap, top) = if rows.len() > 3 {
                ((46.0 * s) as i32, (7.0 * s) as i32, oy + (106.0 * s) as i32)
            } else {
                ((56.0 * s) as i32, (10.0 * s) as i32, oy + (112.0 * s) as i32)
            };
            for (i, kind) in rows.iter().enumerate() {
                let kind = *kind;
                let ry = top + i as i32 * (row_h + row_gap);
                let focused = i == sel;
                let enabled = self.enabled(kind);
                let red = kind == RowKind::PowerOff;
                let fill = if !enabled {
                    if red { RED_DIM } else { GREEN_DIM }
                } else if focused {
                    if red { RED } else { GREEN }
                } else {
                    SLATE_ROW
                };
                // Disabled rows stay fully opaque — the darkness IS the
                // disabled signal.
                cv.fill(margin, ry, rw, row_h, fill, 255);
                // A disabled row can still hold the cursor: the fill stays
                // dim (disabled wins), a thin outline carries the focus.
                if focused && !enabled {
                    let bw = 1.max((2.0 * s) as i32);
                    cv.blend_fill(margin, ry, rw, bw, (255, 255, 255), 120);
                    cv.blend_fill(margin, ry + row_h - bw, rw, bw, (255, 255, 255), 120);
                    cv.blend_fill(margin, ry, bw, row_h, (255, 255, 255), 120);
                    cv.blend_fill(margin + rw - bw, ry, bw, row_h, (255, 255, 255), 120);
                }
                let ty = ry + row_h / 2 + (9.0 * s) as i32;
                let ta = if !enabled {
                    if focused { 110 } else { 70 }
                } else if focused {
                    255
                } else {
                    205
                };
                // The cart launcher row: cart icon + name, or the SD badge +
                // a nudge when the slot is empty.
                let mut tx2 = margin + (18.0 * s) as i32;
                let mut label = self.label(kind).to_string();
                if kind == RowKind::Launch {
                    let isz = (32.0 * s) as i32;
                    let row_icon = match &self.cart {
                        Some(c) => {
                            label = c.name.clone();
                            c.icon.as_ref().or(self.sd_icon.as_ref())
                        }
                        None => {
                            label = "insert cartridge".to_string();
                            self.sd_icon.as_ref()
                        }
                    };
                    if let Some(img) = row_icon {
                        cv.blit(img, tx2, ry + (row_h - isz) / 2, isz);
                        tx2 += isz + (12.0 * s) as i32;
                    }
                }
                let mut lsize = 25.0 * s;
                let avail = margin + rw - tx2 - (12.0 * s) as i32;
                while lsize > 14.0 * s && cv.text_width(&self.font, &label, lsize) > avail {
                    lsize *= 0.92;
                }
                cv.text(&self.font, &label, tx2, ty, lsize, ta);
                // Press acknowledgment: the whole row dips dark for a beat.
                if flash == Some(i) {
                    cv.blend_fill(margin, ry, rw, row_h, (0, 0, 0), 110);
                }
            }
        } else if let Some(cart) = &self.cart {
            // Game list: the cart's contents, windowed, cursor-followed.
            let row_h = (44.0 * s) as i32;
            let row_gap = (8.0 * s) as i32;
            let top = oy + (112.0 * s) as i32;
            const VISIBLE: usize = 4;
            let start = if sel >= VISIBLE {
                (sel + 1 - VISIBLE).min(cart.games.len().saturating_sub(VISIBLE))
            } else {
                0
            };
            for (slot, i) in (start..cart.games.len().min(start + VISIBLE)).enumerate() {
                let g = &cart.games[i];
                let ry = top + slot as i32 * (row_h + row_gap);
                let focused = i == sel;
                cv.fill(margin, ry, rw, row_h, if focused { GREEN } else { SLATE_ROW }, 255);
                let mut tx2 = margin + (14.0 * s) as i32;
                if let Some(img) = &g.icon {
                    let isz = (30.0 * s) as i32;
                    cv.blit(img, tx2, ry + (row_h - isz) / 2, isz);
                    tx2 += isz + (12.0 * s) as i32;
                }
                let ty = ry + row_h / 2 + (8.0 * s) as i32;
                let mut lsize = 22.0 * s;
                let avail = margin + rw - tx2 - (12.0 * s) as i32;
                while lsize > 13.0 * s && cv.text_width(&self.font, &g.name, lsize) > avail {
                    lsize *= 0.92;
                }
                cv.text(&self.font, &g.name, tx2, ty, lsize, if focused { 255 } else { 205 });
                if flash == Some(i) {
                    cv.blend_fill(margin, ry, rw, row_h, (0, 0, 0), 110);
                }
            }
            // Overflow chevrons on the right edge of the list.
            let tri = |cv: &mut Canvas, cx: i32, cy: i32, up: bool| {
                let h = (6.0 * s) as i32;
                for r in 0..h {
                    let y = if up { cy + r } else { cy - r };
                    cv.blend_fill(cx - r, y, r * 2 + 1, 1, (255, 255, 255), 150);
                }
            };
            if start > 0 {
                tri(&mut cv, margin + rw - (14.0 * s) as i32, top - (12.0 * s) as i32, true);
            }
            if start + VISIBLE < cart.games.len() {
                let list_bottom = top + VISIBLE as i32 * (row_h + row_gap);
                tri(&mut cv, margin + rw - (14.0 * s) as i32, list_bottom + (6.0 * s) as i32, false);
            }
        }

        // Legend: the dashboard's own button glyphs.
        let ly = oy + sheet_h - (26.0 * s) as i32;
        let gsz = (26.0 * s) as i32;
        let mut lx = margin;
        let mut item = |cv: &mut Canvas, lx: &mut i32, glyph: &Option<Rgba>, label: &str| {
            if let Some(g) = glyph {
                cv.blit(g, *lx, ly - gsz + (6.0 * s) as i32, gsz);
                *lx += gsz + (8.0 * s) as i32;
            }
            cv.text(&self.font, label, *lx, ly, 19.0 * s, 150);
            *lx += cv.text_width(&self.font, label, 19.0 * s) + (26.0 * s) as i32;
        };
        let (ga, gb, gy) = match self.source {
            Source::Pad => (&self.pad_a, &self.pad_b, &self.pad_y),
            Source::Keyboard => (&self.kb_a, &self.kb_b, &self.kb_y),
        };
        item(&mut cv, &mut lx, ga, "select");
        item(&mut cv, &mut lx, gb, "close");
        // Eject joins the legend only while the launcher row is hovered with
        // a cart inserted — the guide's mirror of the dashboard gesture.
        if self.eject_available(sel) {
            item(&mut cv, &mut lx, gy, "eject");
        }
        // Bottom-right of the legend row: who summoned the guide — controller
        // icon, player-color dot, battery. Keyboard opener gets the key hint
        // there instead.
        if let Some((player, batt)) = &self.opener {
            let isz = (26.0 * s) as i32;
            let dot_r = (7.0 * s) as i32;
            let gap = (10.0 * s) as i32;
            let batt_txt = match batt {
                Some((pct, charging)) => format!("{}%{}", pct, if *charging { " +" } else { "" }),
                None => String::new(),
            };
            let tw = if batt_txt.is_empty() {
                0
            } else {
                cv.text_width(&self.font, &batt_txt, 19.0 * s) + gap
            };
            let total = isz + gap + dot_r * 2 + tw;
            let mut cx = inner_r - total;
            let dcy = ly - (7.0 * s) as i32;
            if let Some(icon) = &self.controller_icon {
                cv.blit(icon, cx, ly - isz + (6.0 * s) as i32, isz);
            }
            cx += isz + gap;
            let col = PLAYER_COLORS[(*player).min(3)];
            for dy in -dot_r..=dot_r {
                for dx in -dot_r..=dot_r {
                    if dx * dx + dy * dy <= dot_r * dot_r {
                        cv.over(cx + dot_r + dx, dcy + dy, 255, col.0 as u32, col.1 as u32, col.2 as u32);
                    }
                }
            }
            cx += dot_r * 2 + gap;
            if !batt_txt.is_empty() {
                cv.text(&self.font, &batt_txt, cx, ly, 19.0 * s, 210);
            }
        } else if self.source == Source::Keyboard {
            let hint = "esc · home";
            let kw = cv.text_width(&self.font, hint, 17.0 * s);
            cv.text(&self.font, hint, inner_r - kw, ly, 17.0 * s, 100);
        }
        cv
    }
}

// ------------------------------------------------------------------ main ---

enum Ev {
    /// name, value, index of the dbus target that spoke (≈ player index).
    Input(String, f64, usize),
    MonitorDied,
}

fn spawn_dbus_listener(tx: mpsc::Sender<Ev>) {
    std::thread::spawn(move || {
        let child = Command::new("dbus-monitor")
            .args([
                "--system",
                "type='signal',interface='org.shadowblip.Input.DBusDevice',member='InputEvent'",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            let _ = tx.send(Ev::MonitorDied);
            return;
        };
        let stdout = child.stdout.take().unwrap();
        let reader = BufReader::new(stdout);
        let mut pending: Option<String> = None;
        let mut pad_idx: usize = 0;
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let t = line.trim();
            if let Some(p) = t.find("target/dbus") {
                pad_idx = t[p + 11..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0);
            } else if let Some(name) = t.strip_prefix("string \"") {
                pending = Some(name.trim_end_matches('"').to_string());
            } else if let Some(v) = t.strip_prefix("double ") {
                if let (Some(name), Ok(val)) = (pending.take(), v.parse::<f64>()) {
                    if tx.send(Ev::Input(name, val, pad_idx)).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = child.kill();
        let _ = tx.send(Ev::MonitorDied);
    });
}

struct App {
    overlay: Overlay,
    ui: Ui,
    sfx: Sfx,
    open: bool,
    sel: usize,
    // A pad button closed the menu and is likely still held: hold off the
    // mode switch until THAT button's release. Switching modes mid-press
    // desyncs InputPlumber's per-button latch and it eats the next press —
    // first seen on Guide, then again on A ("return to game takes two
    // presses"). Stores (event name to wait for, close time).
    defer_intercept: Option<(String, Instant)>,
    // dbus target index of the most recent pad event ≈ player index.
    last_pad_idx: usize,
    // Cart hot-plug watch while the menu is open on the dashboard.
    last_cart_scan: Instant,
    // In-game controller toasts.
    known_pads: Vec<usize>,
    pads_initialized: bool,
    batt_warned: [bool; 4],
    last_pad_scan: Instant,
    toast: Option<(Canvas, i16, i16, Instant)>,
    toast_queue: Vec<Canvas>,
    // Keep repainting LEDs briefly after a pad change: the LED sysfs node can
    // register a moment after the composite device appears.
    repaint_leds_until: Option<Instant>,
}

impl App {
    fn make_toast(&self, text: &str, player: usize) -> Canvas {
        let s = self.overlay.sh as f32 / 1080.0;
        let f = &self.ui.font;
        let th = (52.0 * s) as usize;
        let isz = (28.0 * s) as i32;
        let dot_r = (7.0 * s) as i32;
        let gap = (10.0 * s) as i32;
        let pad = (16.0 * s) as i32;
        let tmp = Canvas::new(1, 1);
        let tw_text = tmp.text_width(f, text, 20.0 * s);
        let tw = (pad + isz + gap + dot_r * 2 + gap + tw_text + pad) as usize;
        let mut cv = Canvas::new(tw, th);
        cv.fill(0, 0, tw as i32, th as i32, SLATE, 255);
        cv.fill(0, th as i32 - 1.max((3.0 * s) as i32), tw as i32, (3.0 * s) as i32, GREEN, 255);
        let mut cx = pad;
        if let Some(icon) = &self.ui.controller_icon {
            cv.blit(icon, cx, (th as i32 - isz) / 2, isz);
        }
        cx += isz + gap;
        let col = PLAYER_COLORS[player.min(3)];
        let cy = th as i32 / 2;
        for dy in -dot_r..=dot_r {
            for dx in -dot_r..=dot_r {
                if dx * dx + dy * dy <= dot_r * dot_r {
                    cv.over(cx + dot_r + dx, cy + dy, 255, col.0 as u32, col.1 as u32, col.2 as u32);
                }
            }
        }
        cx += dot_r * 2 + gap;
        cv.text(f, text, cx, cy + (7.0 * s) as i32, 20.0 * s, 235);
        cv
    }

    /// Cart toast: the SD badge + text, no player dot.
    fn make_cart_toast(&self, text: &str) -> Canvas {
        let s = self.overlay.sh as f32 / 1080.0;
        let f = &self.ui.font;
        let th = (52.0 * s) as usize;
        let isz = (28.0 * s) as i32;
        let gap = (10.0 * s) as i32;
        let pad = (16.0 * s) as i32;
        let tmp = Canvas::new(1, 1);
        let tw_text = tmp.text_width(f, text, 20.0 * s);
        let tw = (pad + isz + gap + tw_text + pad) as usize;
        let mut cv = Canvas::new(tw, th);
        cv.fill(0, 0, tw as i32, th as i32, SLATE, 255);
        cv.fill(0, th as i32 - 1.max((3.0 * s) as i32), tw as i32, (3.0 * s) as i32, GREEN, 255);
        let mut cx = pad;
        if let Some(icon) = &self.ui.sd_icon {
            cv.blit(icon, cx, (th as i32 - isz) / 2, isz);
        }
        cx += isz + gap;
        cv.text(f, text, cx, th as i32 / 2 + (7.0 * s) as i32, 20.0 * s, 235);
        cv
    }

    /// Show now, or hold until the guide closes (the dim would eat it).
    fn push_toast(&mut self, cv: Canvas) {
        if self.open || self.toast.is_some() {
            self.toast_queue.push(cv);
        } else {
            self.show_toast(cv);
        }
    }

    fn show_toast(&mut self, cv: Canvas) {
        let s = self.overlay.sh as f32 / 1080.0;
        let x = ((self.overlay.sw as i32 - cv.w as i32) / 2) as i16;
        let y = (self.overlay.sh as i32 - cv.h as i32 - (48.0 * s) as i32) as i16;
        let _ = self.overlay.ensure_mapped();
        self.sfx.play("toast.wav");
        for f in 1..=5 {
            let a = f as f32 / 5.0;
            let _ = self.overlay.put_at(&cv.frame_scaled(1.0, a), x, y);
            std::thread::sleep(Duration::from_millis(16));
        }
        let _ = self.overlay.put_at(&cv, x, y);
        self.toast = Some((cv, x, y, Instant::now() + Duration::from_millis(2800)));
    }

    /// Expire the active toast and promote the next queued one.
    fn tick_toast(&mut self) {
        if let Some((cv, x, y, until)) = &self.toast {
            if Instant::now() >= *until {
                for f in (1..4).rev() {
                    let a = f as f32 / 4.0;
                    let _ = self.overlay.put_at(&cv.frame_scaled(1.0, a), *x, *y);
                    std::thread::sleep(Duration::from_millis(16));
                }
                let _ = self.overlay.clear_rect(*x, *y, cv.w as u16, cv.h as u16);
                self.toast = None;
            }
        }
        if self.toast.is_none() && !self.open && !self.toast_queue.is_empty() {
            let cv = self.toast_queue.remove(0);
            self.show_toast(cv);
        }
    }

    /// Watch InputPlumber's composite devices: connects, disconnects, and
    /// low batteries become toasts — in-game only, since the bios already
    /// announces controllers on the dashboard.
    fn scan_pads(&mut self) {
        let pads = list_composites();
        if !self.pads_initialized {
            self.pads_initialized = true;
            self.known_pads = pads;
            return;
        }
        if pads != self.known_pads {
            // A fresh CompositeDevice starts at InterceptMode 0 — without
            // this re-assert, a replugged pad's Guide button goes to the game
            // and the daemon never hears it again.
            if self.open {
                set_intercept(2);
            } else if self.defer_intercept.is_none() {
                set_intercept(1);
            }
        }
        if pads != self.known_pads && !dashboard_running() {
            // In-game LED restoration (the bios painter is dead during games).
            self.repaint_leds_until = Some(Instant::now() + Duration::from_secs(6));
            paint_pad_leds();
            let added: Vec<usize> =
                pads.iter().filter(|p| !self.known_pads.contains(p)).cloned().collect();
            let removed: Vec<usize> =
                self.known_pads.iter().filter(|p| !pads.contains(p)).cloned().collect();
            for p in added {
                let txt = match read_battery(p) {
                    Some((pct, _)) => format!("Controller {} connected · {}%", p + 1, pct),
                    None => format!("Controller {} connected", p + 1),
                };
                let cv = self.make_toast(&txt, p);
                self.push_toast(cv);
            }
            for p in removed {
                let cv = self.make_toast(&format!("Controller {} disconnected", p + 1), p);
                self.push_toast(cv);
                self.batt_warned[p.min(3)] = false;
            }
        }
        // Low battery: warn once per discharge cycle, rearm on charge/refill.
        if !dashboard_running() {
            for &p in &pads.clone() {
                if let Some((pct, charging)) = read_battery(p) {
                    let slot = p.min(3);
                    if charging || pct >= 25 {
                        self.batt_warned[slot] = false;
                    } else if pct <= 15 && !self.batt_warned[slot] {
                        self.batt_warned[slot] = true;
                        let cv =
                            self.make_toast(&format!("Controller {} battery low · {}%", p + 1, pct), p);
                        self.push_toast(cv);
                    }
                }
            }
        }
        self.known_pads = pads;
    }
}

impl App {
    fn open_menu(&mut self) {
        // Context can change between opens (dashboard vs game, pad swaps).
        self.ui.dashboard = dashboard_running();
        self.ui.mode = UiMode::Menu;
        self.ui.game_sel = 0;
        self.ui.cart = if self.ui.dashboard { scan_cart() } else { None };
        self.ui.game = if self.ui.dashboard {
            None
        } else {
            resolve_game().map(|(name, icon)| {
                let img = icon.and_then(|p| std::fs::read(p).ok()).and_then(|b| decode_png(&b));
                (name, img)
            })
        };
        let brand = pad_brand();
        let (a, b, y) = if brand.contains("DualSense") || brand.contains("Sony") || brand.contains("PlayStation") {
            (GLYPH_PS_CROSS, GLYPH_PS_CIRCLE, GLYPH_PS_TRIANGLE)
        } else if brand.contains("Switch") || brand.contains("Nintendo") {
            (GLYPH_SW_A, GLYPH_SW_B, GLYPH_SW_X)
        } else {
            (GLYPH_XBOX_A, GLYPH_XBOX_B, GLYPH_XBOX_Y)
        };
        self.ui.pad_a = decode_png(a);
        self.ui.pad_b = decode_png(b);
        self.ui.pad_y = decode_png(y);
        self.ui.opener = if self.ui.source == Source::Pad {
            Some((self.last_pad_idx, read_battery(self.last_pad_idx)))
        } else {
            None
        };

        self.open = true;
        self.sel = 0;
        self.defer_intercept = None;
        // The dim paints over any visible toast; it re-queues implicitly by
        // simply being dropped (short-lived, not worth restoring).
        self.toast = None;
        set_intercept(2);
        self.overlay.grab_keyboard();
        self.sfx.play("select.wav");
        let _ = self.overlay.ensure_mapped();
        let _ = self.overlay.backdrop();
        let panel = self.ui.render(self.overlay.pw, self.overlay.ph, self.overlay.sh, self.sel, None);
        for f in 1..=6 {
            let t = smoothstep(f as f32 / 6.0);
            let _ = self.overlay.put_panel(&panel.frame_scaled(0.90 + 0.10 * t, t));
            std::thread::sleep(Duration::from_millis(14));
        }
        let _ = self.overlay.put_panel(&panel);
    }

    /// `release_of`: the pad event whose release must arrive before the
    /// intercept switch (None = keyboard close, switch immediately).
    fn close_menu(&mut self, release_of: Option<String>) {
        self.open = false;
        self.ui.mode = UiMode::Menu;
        self.overlay.ungrab_keyboard();
        self.sfx.play("back.wav");
        let panel = self.ui.render(self.overlay.pw, self.overlay.ph, self.overlay.sh, self.sel, None);
        // Frames overwrite in place (each carries its own transparent
        // margins), so no clear between them — clearing per frame is what
        // made the close animation flicker.
        for f in (1..4).rev() {
            let t = smoothstep(f as f32 / 4.0);
            let _ = self.overlay.put_panel(&panel.frame_scaled(0.90 + 0.10 * t, t));
            std::thread::sleep(Duration::from_millis(14));
        }
        let _ = self.overlay.clear();
        match release_of {
            Some(name) => self.defer_intercept = Some((name, Instant::now())),
            None => set_intercept(1),
        }
    }

    /// The event whose release gates the mode switch, when a pad button is
    /// doing the closing.
    fn pad_release(&self, action: &str) -> Option<String> {
        if self.ui.source != Source::Pad {
            return None;
        }
        match action {
            "toggle" => Some("ui_guide".into()),
            "back" => Some("ui_back".into()),
            "accept" => Some("ui_accept".into()),
            _ => None,
        }
    }

    /// The cursor index for whichever list is showing.
    fn cur_sel(&self) -> usize {
        if self.ui.mode == UiMode::Games { self.ui.game_sel } else { self.sel }
    }

    fn redraw(&self) {
        let panel =
            self.ui.render(self.overlay.pw, self.overlay.ph, self.overlay.sh, self.cur_sel(), None);
        let _ = self.overlay.put_panel(&panel);
    }

    /// Press acknowledgment: the focused row dips dark for a beat before the
    /// action lands.
    fn flash_row(&self) {
        let cs = self.cur_sel();
        let panel = self.ui.render(self.overlay.pw, self.overlay.ph, self.overlay.sh, cs, Some(cs));
        let _ = self.overlay.put_panel(&panel);
        std::thread::sleep(Duration::from_millis(110));
    }

    /// Launch a kzi from the dashboard guide: hand the session the launch
    /// command and step out of the way. The daemon exits; the game block's
    /// fresh daemon takes over once the game is up.
    fn do_launch(&mut self, kzi: PathBuf) -> bool {
        log(&format!("action: launch {}", kzi.display()));
        self.sfx.play("launch.wav");
        self.flash_row();
        set_intercept(0);
        let _ = self.overlay.clear();
        launch_kzi(&kzi);
        false
    }

    fn step_sel(&mut self, dir: i32) {
        // Disabled rows still take the cursor — they reject on press instead.
        let n = self.ui.rows().len() as i32;
        self.sel = ((self.sel as i32 + dir + n) % n) as usize;
        self.sfx.play("move.wav");
        self.redraw();
    }

    /// Live legend: whichever device spoke last owns the glyphs, exactly
    /// like the dashboard's input_state.last_source.
    fn note_source(&mut self, src: Source) {
        if self.ui.source != src {
            self.ui.source = src;
            if self.open {
                self.redraw();
            }
        }
    }

    /// Returns false to exit the daemon.
    fn act(&mut self, action: &str) -> bool {
        if !self.open {
            if action == "toggle" {
                self.open_menu();
            }
            return true;
        }
        // Game-list sub-menu: B backs out to the main rows, Guide still
        // closes everything.
        if self.ui.mode == UiMode::Games {
            match action {
                "toggle" => {
                    let r = self.pad_release("toggle");
                    self.close_menu(r);
                }
                "back" => {
                    self.ui.mode = UiMode::Menu;
                    self.sfx.play("back.wav");
                    self.redraw();
                }
                "up" | "down" => {
                    let n = self.ui.cart.as_ref().map(|c| c.games.len()).unwrap_or(0);
                    if n > 0 {
                        let d = if action == "up" { n - 1 } else { 1 };
                        self.ui.game_sel = (self.ui.game_sel + d) % n;
                        self.sfx.play("move.wav");
                        self.redraw();
                    }
                }
                "accept" => {
                    if let Some(kzi) = self
                        .ui
                        .cart
                        .as_ref()
                        .and_then(|c| c.games.get(self.ui.game_sel))
                        .map(|g| g.kzi.clone())
                    {
                        return self.do_launch(kzi);
                    }
                }
                _ => {}
            }
            return true;
        }
        match action {
            "toggle" => {
                let r = self.pad_release("toggle");
                self.close_menu(r);
            }
            "back" => {
                let r = self.pad_release("back");
                self.close_menu(r);
            }
            "up" => self.step_sel(-1),
            "down" => self.step_sel(1),
            // North face (Y / Triangle) or keyboard E — the dashboard's own
            // eject gesture, honored while the launcher row holds a cart.
            "eject" if self.ui.eject_available(self.sel) => {
                log("action: eject cart");
                self.sfx.play("back.wav");
                self.flash_row();
                let _ = Command::new("sudo")
                    .args(["-n", "/usr/bin/kazeta-eject"])
                    .spawn();
                // The 1s cart watch flips the launcher row to "insert
                // cartridge" once the mount actually disappears; the toast
                // queues until the guide closes.
                self.last_cart_scan = Instant::now();
                let cv = self.make_cart_toast("Cart Ejected - Safe to Remove");
                self.push_toast(cv);
                self.redraw();
            }
            "eject" => {}
            "accept" if !self.ui.enabled(self.ui.rows()[self.sel]) => {
                self.sfx.play("reject.wav");
                self.flash_row();
                self.redraw();
            }
            "accept" => match self.ui.rows()[self.sel] {
                RowKind::Launch => {
                    // The cart launcher: single game launches, a collection
                    // opens the game list.
                    let games: Vec<PathBuf> = self
                        .ui
                        .cart
                        .as_ref()
                        .map(|c| c.games.iter().map(|g| g.kzi.clone()).collect())
                        .unwrap_or_default();
                    match games.len() {
                        0 => {}
                        1 => return self.do_launch(games[0].clone()),
                        _ => {
                            self.sfx.play("select.wav");
                            self.flash_row();
                            self.ui.mode = UiMode::Games;
                            self.ui.game_sel = 0;
                            self.redraw();
                        }
                    }
                }
                RowKind::ReturnToGame => {
                    self.flash_row();
                    let r = self.pad_release("accept");
                    self.close_menu(r);
                }
                RowKind::KazetaHome => {
                    log("action: kazeta home");
                    self.sfx.play("select.wav");
                    self.flash_row();
                    set_intercept(0);
                    let _ = self.overlay.clear();
                    kill_game();
                    return false;
                }
                RowKind::PowerOff => {
                    log("action: power off");
                    self.flash_row();
                    set_intercept(0);
                    let _ = Command::new("systemctl").arg("poweroff").status();
                    return false;
                }
            },
            _ => {}
        }
        true
    }
}

fn main() {
    log("=== kazeta-guided start ===");

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        set_intercept(0);
        default_hook(info);
    }));

    let font = match find_font() {
        Some(f) => f,
        None => {
            log("no font found, exiting");
            return;
        }
    };

    let Some(overlay) = Overlay::connect_with_retry() else {
        log("no X server after retries, exiting");
        return;
    };
    overlay.grab_home();

    let (tx, rx) = mpsc::channel();
    spawn_dbus_listener(tx);

    set_intercept(1);

    let ui = Ui {
        font,
        pad_a: None,
        pad_b: None,
        pad_y: None,
        kb_a: decode_png(GLYPH_KB_ENTER),
        kb_b: decode_png(GLYPH_KB_BACK),
        kb_y: decode_png(GLYPH_KB_E),
        controller_icon: decode_png(ICON_CONTROLLER),
        sd_icon: decode_png(ICON_SDCARD),
        source: Source::Pad,
        game: None,
        dashboard: false,
        cart: None,
        mode: UiMode::Menu,
        game_sel: 0,
        opener: None,
    };
    let mut app = App {
        overlay,
        ui,
        sfx: Sfx::find(),
        open: false,
        sel: 0,
        defer_intercept: None,
        last_pad_idx: 0,
        last_cart_scan: Instant::now(),
        known_pads: Vec::new(),
        pads_initialized: false,
        batt_warned: [false; 4],
        last_pad_scan: Instant::now(),
        toast: None,
        toast_queue: Vec::new(),
        repaint_leds_until: None,
    };

    loop {
        loop {
            match app.overlay.conn.poll_for_event() {
                Ok(Some(x11rb::protocol::Event::Expose(_))) => {
                    if app.open {
                        let _ = app.overlay.backdrop();
                        app.redraw();
                    } else if app.overlay.mapped {
                        let _ = app.overlay.clear();
                    }
                }
                Ok(Some(x11rb::protocol::Event::KeyPress(k))) => {
                    let action = match app.overlay.keysym_for(k.detail) {
                        KS_HOME => Some("toggle"),
                        KS_UP => Some("up"),
                        KS_DOWN => Some("down"),
                        KS_RETURN => Some("accept"),
                        KS_ESCAPE | KS_BACKSPACE => Some("back"),
                        KS_E => Some("eject"),
                        _ => None,
                    };
                    if let Some(a) = action {
                        log(&format!("key {} kc={} (open={})", a, k.detail, app.open));
                        app.note_source(Source::Keyboard);
                        // Keyboard closes are never guide-held closes.
                        let a = if a == "toggle" && app.open { "back" } else { a };
                        if !app.act(a) {
                            return;
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    log("X connection lost, exiting");
                    set_intercept(0);
                    return;
                }
            }
        }

        // Fallback for a deferred mode switch whose release never arrived
        // (e.g. the pad disconnected mid-press).
        if let Some((_, t)) = &app.defer_intercept {
            if t.elapsed() > Duration::from_millis(600) {
                app.defer_intercept = None;
                set_intercept(1);
            }
        }

        let ev = match rx.recv_timeout(Duration::from_millis(80)) {
            Ok(ev) => ev,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Live cart row: rescan mounted media while the menu is open
                // on the dashboard, so inserting or pulling a card updates
                // the launcher without reopening the guide.
                if app.open
                    && app.ui.dashboard
                    && app.last_cart_scan.elapsed() > Duration::from_secs(1)
                {
                    app.last_cart_scan = Instant::now();
                    let fresh = scan_cart();
                    let key = |c: &Option<Cart>| c.as_ref().map(|c| (c.name.clone(), c.games.len()));
                    if key(&fresh) != key(&app.ui.cart) {
                        app.ui.cart = fresh;
                        let n = app.ui.cart.as_ref().map(|c| c.games.len()).unwrap_or(0);
                        if app.ui.game_sel >= n {
                            app.ui.game_sel = 0;
                        }
                        if n == 0 && app.ui.mode == UiMode::Games {
                            app.ui.mode = UiMode::Menu;
                        }
                        app.redraw();
                    }
                }
                // Controller watch + toast lifecycle.
                if app.last_pad_scan.elapsed() > Duration::from_secs(2) {
                    app.last_pad_scan = Instant::now();
                    app.scan_pads();
                    if let Some(until) = app.repaint_leds_until {
                        if Instant::now() < until {
                            paint_pad_leds();
                        } else {
                            app.repaint_leds_until = None;
                        }
                    }
                }
                app.tick_toast();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                set_intercept(0);
                return;
            }
        };

        match ev {
            Ev::MonitorDied => {
                log("dbus-monitor died, exiting");
                set_intercept(0);
                return;
            }
            Ev::Input(name, val, pad_idx) => {
                if val < 0.5 {
                    if app.defer_intercept.as_ref().map(|(n, _)| n == &name).unwrap_or(false) {
                        app.defer_intercept = None;
                        set_intercept(1);
                    }
                    continue;
                }
                app.last_pad_idx = pad_idx;
                app.note_source(Source::Pad);
                log(&format!("pad {} {} (open={} sel={})", name, val, app.open, app.sel));
                let action = match name.as_str() {
                    "ui_guide" => "toggle",
                    "ui_back" => "back",
                    "ui_up" => "up",
                    "ui_down" => "down",
                    "ui_accept" => "accept",
                    // The north face arrives under different names across
                    // InputPlumber versions; none of these are used elsewhere.
                    "ui_context" | "ui_osk" | "ui_action" => "eject",
                    _ => continue,
                };
                if !app.act(action) {
                    return;
                }
            }
        }
    }
}

fn find_font() -> Option<fontdue::Font> {
    let candidates = [
        "/home/gamer/.local/share/kazeta-plus/themes/Metro360/segoeui.ttf",
        "/usr/share/kazeta-plus/themes/Metro360/segoeui.ttf",
    ];
    for c in candidates {
        if let Ok(bytes) = std::fs::read(c) {
            if let Ok(f) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                log(&format!("font: {}", c));
                return Some(f);
            }
        }
    }
    for root in ["/home/gamer/.local/share/kazeta-plus/themes", "/usr/share/kazeta-plus/themes"] {
        let Ok(themes) = std::fs::read_dir(root) else { continue };
        for theme in themes.flatten() {
            let Ok(files) = std::fs::read_dir(theme.path()) else { continue };
            for e in files.flatten() {
                if e.path().extension().map(|x| x == "ttf").unwrap_or(false) {
                    if let Ok(bytes) = std::fs::read(e.path()) {
                        if let Ok(f) =
                            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                        {
                            log(&format!("font (fallback): {:?}", e.path()));
                            return Some(f);
                        }
                    }
                }
            }
        }
    }
    None
}
