// Metro (2011+ Xbox 360 dashboard) main menu: lowercase tab strip up top,
// panes of flat tiles below — one big hero tile plus small tiles in a 2-row
// quad. Left/right walks tiles and crosses pane edges (or prev/next jumps a
// whole tab), up/down moves within a small-tile column, select activates.
// Enabled via `menu_style = "METRO"` in config.toml or a theme's theme.toml.

use crate::{
    Screen, InputState, render_background, render_ui_overlay_alpha, get_current_font, measure_text,
    text_with_config_color, string_to_color, FONT_SIZE,
    StorageMediaState, VideoPlayer, save,
    Memory, PlaytimeCache, SizeCache, CopyOperationState, Dialog,
    audio::SoundEffects,
    config::Config,
    memory::{get_game_playtime, get_game_size},
    types::{AnimationState, BackgroundState, BatteryInfo, DialogState, ShakeTarget, UIFocus},
    ui::text_with_color,
    ui::blades::BladeAction,
    ui::main_menu::{activate_copy_logs, activate_play, activate_save_data},
};
use crate::audio::AUDIO;
use crate::input::InputSource;
use macroquad::prelude::*;
use rodio::{buffer::SamplesBuffer, Decoder, Sink, Source};
use std::io::Cursor;
use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    sync::atomic::Ordering,
};

// ===================================
// TAB / TILE DEFINITIONS
// ===================================

pub struct MetroTile {
    pub label: &'static str,
    pub action: BladeAction,
    pub hero: bool,  // the one big 920x430 banner tile (Play)
    pub green: bool, // Xbox-green fill; false = alternating slate
    pub col: u8,     // grid column; a column holding the hero is banner-width
    pub row: u8,     // 0 = top, 1 = bottom (heroes span both rows)
}

pub struct MetroTab {
    pub title: &'static str,
    pub tiles: &'static [MetroTile],
}

pub const TABS: &[MetroTab] = &[
    MetroTab {
        title: "home",
        // Two small tiles, then the Play banner with open space to its right.
        tiles: &[
            MetroTile { label: "Save Data", action: BladeAction::SaveData, hero: false, green: true, col: 0, row: 0 },
            MetroTile { label: "Runtimes", action: BladeAction::RuntimeDownloader, hero: false, green: false, col: 0, row: 1 },
            MetroTile { label: "Play", action: BladeAction::Play, hero: true, green: true, col: 1, row: 0 },
        ],
    },
    MetroTab {
        title: "music",
        tiles: &[
            MetroTile { label: "CD Player", action: BladeAction::CdPlayer, hero: false, green: true, col: 0, row: 0 },
        ],
    },
    MetroTab {
        title: "settings",
        tiles: &[
            MetroTile { label: "Settings", action: BladeAction::Settings, hero: false, green: true, col: 0, row: 0 },
            MetroTile { label: "Wi-Fi", action: BladeAction::Wifi, hero: false, green: false, col: 0, row: 1 },
            MetroTile { label: "Bluetooth", action: BladeAction::Bluetooth, hero: false, green: false, col: 1, row: 0 },
            MetroTile { label: "About", action: BladeAction::About, hero: false, green: false, col: 1, row: 1 },
            MetroTile { label: "Session Logs", action: BladeAction::CopyLogs, hero: false, green: false, col: 2, row: 0 },
            MetroTile { label: "Updates", action: BladeAction::UpdateChecker, hero: false, green: false, col: 2, row: 1 },
            MetroTile { label: "Themes", action: BladeAction::ThemeDownloader, hero: false, green: true, col: 3, row: 0 },
        ],
    },
];

const DEFAULT_TAB: usize = 0; // home
// Left edge of the tab strip and panes, 360p units. Sized so home's full
// row (wide small + hero banner + wide small) keeps a margin on the right
// even with the 1.07x focus scale on the last column.
const ORIGIN_X: f32 = 52.0;

// Focus animation, timings straight from dashx360's MetroTile control.
const SEL_SCALE: f32 = 1.07;     // focused tiles grow 7%
const SEL_GROW_TIME: f32 = 0.16; // cubic ease-out in
const SEL_SHRINK_TIME: f32 = 0.12; // sine ease-out back
const PRESS_FLASH_TIME: f32 = 0.12; // press acknowledgment dip

/// The tile a tab should land on when jumped to directly: its hero if it has
/// one, else the first tile.
fn primary_tile(tab: usize) -> usize {
    TABS[tab].tiles.iter().position(|t| t.hero).unwrap_or(0)
}
const SLIDE_TIME: f32 = 0.25; // pane slide duration, seconds

// Xbox green and the flat Metro tile palette.
const XBOX_GREEN: Color = Color::new(0.063, 0.486, 0.063, 1.0);
const TILE_SLATE: Color = Color::new(0.24, 0.25, 0.26, 1.0);
const TILE_SLATE_ALT: Color = Color::new(0.30, 0.31, 0.33, 1.0);
const BG_FALLBACK: Color = Color::new(0.12, 0.12, 0.12, 1.0);

// ===================================
// STATE
// ===================================

pub struct MetroState {
    pub tab: usize,   // active tab
    pub prev_tab: usize,
    pub anim: f32,    // 0..1 pane slide progress, 1 = settled
    pub dir: f32,     // -1 slide came from the left, +1 from the right
    pub tile: usize,  // selected tile on the active tab
    // Cart branding for the Play hero (cover art + "Play: NAME" bar).
    pub cover_tex: Option<Texture2D>,
    pub icon_tex: Option<Texture2D>,
    // Blurred cover used as an ambient background, crossfaded in while the
    // Play hero is hovered and back out when it isn't.
    cover_blur: Option<Texture2D>,
    cover_bg_vis: f32,
    // Loudness envelope of the cart's theme (built off-thread) plus the clock
    // it started on, so the dashboard can move to the music while hovering.
    bgm_env: Arc<Mutex<Option<Vec<f32>>>>,
    bgm_start: f64,
    beat: f32,
    pub cart_label: Option<String>,
    pub cart_optical: bool,
    // Media badges (baked-in art) for the hero's corner.
    badge_sd: Texture2D,
    badge_disc: Texture2D,
    cover_key: String, // change marker so we only reload when the cart changes
    // Insert/eject animation: the current branding fades in as cart_vis walks
    // 0..1; on eject/swap the old branding is parked in `outgoing` and fades
    // out on its own clock (a fast swap therefore crossfades).
    cart_vis: f32,
    outgoing: Option<OutgoingBrand>,
    // Focus animation: seconds since the cursor moved, which tile is
    // shrinking back, and the press-dip envelope (1..0 after a select).
    sel_anim: f32,
    prev_sel: Option<usize>,
    press_flash: f32,
    // Boot intro (0..1): runs once the first time the dash is on screen —
    // update() only ticks while Metro is the active screen, so the clock
    // naturally starts when the splash hands over.
    intro_t: f32,
    // Outro (0..1 when Some): the reverse choreography that plays after a
    // tile with a departure animation is chosen; the action fires when it
    // completes.
    outro_t: Option<f32>,
    outro_action: OutroAction,
    // Boot screen: true from game spawn until its process exits — the dash
    // shows a spinner instead of sliding back in behind the game.
    booting: bool,
    // Which confirm-button glyph the legend shows, tracking the last-used
    // input device (keyboard vs pad, and pad brand).
    legend_icon: LegendIcon,
    // Queued player connect/disconnect toasts, shown one at a time.
    toasts: Vec<Toast>,
    // Cart presence last frame. None until the first update, so a cart that
    // is already in at boot doesn't announce itself.
    had_cart: Option<bool>,
    // Save icons for the Save Data tile's marquee rows, loaded once at
    // startup from the internal save cache.
    save_icons: Vec<Texture2D>,
    // CONSOLE_ICONS indices for the inserted cart's runtime(s), shown in a
    // row under the Play hero. Refreshed with the cart branding.
    cart_console_icons: Vec<usize>,
    // Ambient bokeh motes (config.background_particles == "ON"): parameters
    // are rolled once at startup, positions are pure functions of time.
    bokeh: Vec<Bokeh>,
    bokeh_tex: Texture2D,
    fade_tex: Texture2D, // vertical alpha ramp for smooth tile gradients
    // Hover bgm (cartinfo.yaml `bgm:`): loops while the Play hero is selected,
    // fading in on hover and out on unhover.
    bgm_path: Option<PathBuf>,
    bgm_sink: Option<Sink>,
    bgm_vol: f32,
    // Mount fingerprint so cart swaps invalidate branding even while the
    // game list is stale (it only rebuilds when the Play screen opens).
    mounts_fp: String,
    mounts_polled: f64,
}

/// Snapshot of the previous cart's hero branding, kept alive just long enough
/// to animate out after an eject or swap.
struct OutgoingBrand {
    cover: Option<Texture2D>,
    icon: Option<Texture2D>,
    label: Option<String>,
    optical: bool,
    vis: f32, // 1..0, drops to zero then the snapshot is discarded
}

/// One drifting background mote. Everything is rolled once; the draw pass
/// derives position/alpha purely from elapsed time, so no per-frame state.
struct Bokeh {
    x: f32,          // 0..1 across the screen
    y: f32,          // 0..1 down the screen (drifts upward, wraps)
    r: f32,          // radius in 360p design units
    speed: f32,      // upward drift, screen heights per second
    wobble: f32,     // sideways sway amplitude, screen widths
    wobble_hz: f32,
    twinkle_hz: f32, // how fast the mote breathes in and out
    phase: f32,      // personal offset so motes never sync up
    alpha: f32,      // peak alpha, well under 0.15 — these are ambience
}

const BGM_FADE_TIME: f32 = 0.7; // seconds for a full fade in or out
const CART_ANIM_TIME: f32 = 0.4; // seconds for cart branding to fade in or out
const BOKEH_COUNT: usize = 30;
const INTRO_TIME: f32 = 1.0; // boot choreography after the splash video
const OUTRO_TIME: f32 = 0.7; // reverse choreography when Play is chosen
const TOAST_TIME: f32 = 2.8; // player connect/disconnect pill lifetime

/// What fires when the outro choreography lands.
#[derive(Clone, Copy, PartialEq)]
enum OutroAction {
    Play,
    SaveData,
}

/// One notification pill queued for the bottom of the screen.
struct Toast {
    text: String,
    dot: Option<Color>, // player slot LED color; None hides the dot
    icon: ToastIcon,
    t: f32,
}

#[derive(Clone, Copy)]
enum ToastIcon {
    Pad,  // controller silhouette
    Cart, // SD card badge
}

/// Confirm-button glyph shown in the legend, by last-used device.
#[derive(Clone, Copy, PartialEq)]
pub enum LegendIcon {
    Keyboard = 0,
    Xbox = 1,
    PlayStation = 2,
    Switch = 3,
    Switch2 = 4,
    Steam = 5,
    N64 = 6,
}

thread_local! {
    // Button glyphs baked into the binary; index matches LegendIcon.
    static LEGEND_ICONS: [Texture2D; 7] = [
        legend_tex(include_bytes!("../../buttons/keyboard_enter.png")),
        legend_tex(include_bytes!("../../buttons/xbox_button_color_a.png")),
        legend_tex(include_bytes!("../../buttons/playstation_button_color_cross.png")),
        legend_tex(include_bytes!("../../buttons/switch1_button_a.png")),
        legend_tex(include_bytes!("../../buttons/switch2_button_a.png")),
        legend_tex(include_bytes!("../../buttons/steam_button_color_a.png")),
        legend_tex(include_bytes!("../../buttons/n64_button_a.png")),
    ];
    // Matching back-button glyphs (B / circle / backspace), same indexing.
    static LEGEND_ICONS_BACK: [Texture2D; 7] = [
        legend_tex(include_bytes!("../../buttons/keyboard_backspace.png")),
        legend_tex(include_bytes!("../../buttons/xbox_button_color_b.png")),
        legend_tex(include_bytes!("../../buttons/playstation_button_color_circle.png")),
        legend_tex(include_bytes!("../../buttons/switch1_button_b.png")),
        legend_tex(include_bytes!("../../buttons/switch2_button_b.png")),
        legend_tex(include_bytes!("../../buttons/steam_button_color_b.png")),
        legend_tex(include_bytes!("../../buttons/n64_button_b.png")),
    ];
    // Top-face glyphs (Y / Triangle / X / C-Up / E) for the Eject action.
    static LEGEND_ICONS_EJECT: [Texture2D; 7] = [
        legend_tex(include_bytes!("../../buttons/keyboard_e.png")),
        legend_tex(include_bytes!("../../buttons/xbox_button_color_y.png")),
        legend_tex(include_bytes!("../../buttons/playstation_button_color_triangle.png")),
        legend_tex(include_bytes!("../../buttons/switch1_button_x.png")),
        legend_tex(include_bytes!("../../buttons/switch2_button_x.png")),
        legend_tex(include_bytes!("../../buttons/steam_button_color_y.png")),
        legend_tex(include_bytes!("../../buttons/n64_button_cup.png")),
    ];
}

fn legend_tex(bytes: &[u8]) -> Texture2D {
    let tex = Texture2D::from_file_with_format(bytes, Some(ImageFormat::Png));
    tex.set_filter(FilterMode::Linear);
    tex
}

thread_local! {
    // Toast furniture: neutral controller silhouette + a smooth tintable dot
    // (draw_circle's small polygons look pixelated at dot sizes).
    static TOAST_PAD_ICON: Texture2D = legend_tex(include_bytes!("../../CONTROLLER.png"));
    static TOAST_DOT: Texture2D = make_dot_texture();
    // Tile face icons.
    static TILE_WIFI: Texture2D = legend_tex(include_bytes!("../../WIFI.png"));
    static TILE_BLUETOOTH: Texture2D = legend_tex(include_bytes!("../../BLUETOOTH.png"));
    static TILE_SETTINGS: Texture2D = legend_tex(include_bytes!("../../SETTINGS.png"));
    static TILE_ABOUT: Texture2D = legend_tex(include_bytes!("../../ABOUT.png"));
    static TILE_THEMES: Texture2D = legend_tex(include_bytes!("../../THEMES.png"));
    static TILE_UPDATES: Texture2D = legend_tex(include_bytes!("../../UPDATES.png"));
    static TILE_LOGS: Texture2D = legend_tex(include_bytes!("../../LOGS.png"));
    // Iridescent soap bubble the cart's console icons float inside.
    static BUBBLE: Texture2D = legend_tex(include_bytes!("../../BUBBLE.png"));
    // The settings screen draws outside MetroState, whose fade_tex/badge_disc
    // are private fields — it keeps its own copies.
    static FADE_TEX: Texture2D = make_fade_texture();
    static TILE_DISC: Texture2D = legend_tex(include_bytes!("../../DISC.png"));
    // Console-family icons shown under the Play hero, indexed by the CI_*
    // constants below.
    static CONSOLE_ICONS: [Texture2D; 27] = [
        legend_tex(include_bytes!("../../consoles/Arcade.png")),
        legend_tex(include_bytes!("../../consoles/Atari 2600.png")),
        legend_tex(include_bytes!("../../consoles/Dreamcast.png")),
        legend_tex(include_bytes!("../../consoles/DS.png")),
        legend_tex(include_bytes!("../../consoles/Gameboy Color.png")),
        legend_tex(include_bytes!("../../consoles/Gameboy.png")),
        legend_tex(include_bytes!("../../consoles/Gamecube.png")),
        legend_tex(include_bytes!("../../consoles/GameGear.png")),
        legend_tex(include_bytes!("../../consoles/GBA.png")),
        legend_tex(include_bytes!("../../consoles/Genesis.png")),
        legend_tex(include_bytes!("../../consoles/GOG.png")),
        legend_tex(include_bytes!("../../consoles/Master System.png")),
        legend_tex(include_bytes!("../../consoles/N64.png")),
        legend_tex(include_bytes!("../../consoles/NES.png")),
        legend_tex(include_bytes!("../../consoles/PS1.png")),
        legend_tex(include_bytes!("../../consoles/PS2.png")),
        legend_tex(include_bytes!("../../consoles/PS3.png")),
        legend_tex(include_bytes!("../../consoles/PSP.png")),
        legend_tex(include_bytes!("../../consoles/Saturn.png")),
        legend_tex(include_bytes!("../../consoles/SNES.png")),
        legend_tex(include_bytes!("../../consoles/Steam.png")),
        legend_tex(include_bytes!("../../consoles/Switch.png")),
        legend_tex(include_bytes!("../../consoles/Wii.png")),
        legend_tex(include_bytes!("../../consoles/WiiU.png")),
        legend_tex(include_bytes!("../../consoles/Xbox 360.png")),
        legend_tex(include_bytes!("../../consoles/Xbox.png")),
        legend_tex(include_bytes!("../../consoles/PS4.png")),
    ];
}

const CI_ARCADE: usize = 0;
const CI_ATARI: usize = 1;
const CI_DREAMCAST: usize = 2;
const CI_DS: usize = 3;
const CI_GBC: usize = 4;
const CI_GAMEBOY: usize = 5;
const CI_GAMECUBE: usize = 6;
const CI_GAMEGEAR: usize = 7;
const CI_GBA: usize = 8;
const CI_GENESIS: usize = 9;
const CI_GOG: usize = 10;
const CI_MASTER_SYSTEM: usize = 11;
const CI_N64: usize = 12;
const CI_NES: usize = 13;
const CI_PS1: usize = 14;
const CI_PS2: usize = 15;
const CI_PS3: usize = 16;
const CI_PSP: usize = 17;
const CI_SATURN: usize = 18;
const CI_SNES: usize = 19;
const CI_STEAM: usize = 20;
const CI_SWITCH: usize = 21;
const CI_WII: usize = 22;
const CI_WIIU: usize = 23;
const CI_XBOX360: usize = 24;
const CI_XBOX: usize = 25;
const CI_PS4: usize = 26;

/// Cheap deterministic 0..1 hash, for per-appearance jitter that stays
/// stable without storing any state.
fn hash01(seed: u32) -> f32 {
    let x = seed.wrapping_mul(2_654_435_761);
    ((x >> 8) & 0xFFFF) as f32 / 65535.0
}

/// Console icon(s) for a kzi Runtime value. No runtime = plain PC (GOG),
/// Windows-flavored runtimes = Steam, Dolphin covers two consoles.
fn console_icons_for_runtime(rt: &str) -> Vec<usize> {
    let r = rt.to_lowercase();
    if r.is_empty() || r == "none" {
        return vec![CI_GOG];
    }
    if r.contains("dolphin") {
        return vec![CI_GAMECUBE, CI_WII];
    }
    if r.contains("windows") || r.contains("proton") || r.contains("wine") || r.contains("umu") {
        return vec![CI_STEAM];
    }
    // Arcade first: these names are distinctive, and checking them before the
    // console rules keeps a future spelling from tripping a substring like
    // "nes" or "gb" further down.
    if r.contains("mame")
        || r.contains("fbneo")
        || r.contains("fbalpha")
        || r.contains("finalburn")
        || r.contains("final burn")
        || r.contains("neogeo")
        || r.contains("neo geo")
        || r.contains("cps")
        || r.contains("arcade")
    {
        return vec![CI_ARCADE];
    }
    // Specific PlayStations before the generic "playstation" catch-all.
    if r.contains("pcsx2") || r.contains("playstation 2") || r.contains("playstation2") || r.contains("ps2") {
        return vec![CI_PS2];
    }
    if r.contains("rpcs3") || r.contains("playstation 3") || r.contains("ps3") { return vec![CI_PS3]; }
    if r.contains("shadps4") || r.contains("playstation 4") || r.contains("ps4") { return vec![CI_PS4]; }
    if r.contains("duckstation") || r.contains("psx") || r.contains("beetle") || r.contains("playstation") || r.contains("ps1") {
        return vec![CI_PS1];
    }
    if r.contains("ppsspp") || r.contains("psp") { return vec![CI_PSP]; }
    if r.contains("mupen") || r.contains("n64") || r.contains("nintendo64") || r.contains("nintendo 64") || r.contains("parallel") { return vec![CI_N64]; }
    // "snes" before "nes": every snes name contains nes.
    if r.contains("snes") || r.contains("bsnes") { return vec![CI_SNES]; }
    if r.contains("nes") || r.contains("fceumm") || r.contains("mesen") { return vec![CI_NES]; }
    if r.contains("gba") || r.contains("mgba") { return vec![CI_GBA]; }
    if r.contains("gbc") { return vec![CI_GBC]; }
    if r.contains("gambatte") || r.contains("gameboy") || r == "gb" { return vec![CI_GAMEBOY]; }
    if r.contains("genesis") || r.contains("mega") || r.contains("blastem") { return vec![CI_GENESIS]; }
    if r.contains("saturn") { return vec![CI_SATURN]; }
    if r.contains("dreamcast") || r.contains("flycast") || r.contains("redream") { return vec![CI_DREAMCAST]; }
    if r.contains("melonds") || r.contains("desmume") || r.contains("nds") { return vec![CI_DS]; }
    if r.contains("yuzu") || r.contains("ryujinx") || r.contains("eden") || r.contains("switch") { return vec![CI_SWITCH]; }
    if r.contains("xenia") || r.contains("360") { return vec![CI_XBOX360]; }
    if r.contains("xemu") || r.contains("xbox") { return vec![CI_XBOX]; }
    if r.contains("cemu") || r.contains("wiiu") { return vec![CI_WIIU]; }
    if r.contains("atari") || r.contains("stella") { return vec![CI_ATARI]; }
    if r.contains("gamegear") { return vec![CI_GAMEGEAR]; }
    if r.contains("master") || r.contains("sms") { return vec![CI_MASTER_SYSTEM]; }
    // Unknown runtime: call it PC.
    vec![CI_GOG]
}

/// Every Runtime= across the mounted cart's .kzi files, mapped to console
/// icons and deduped in first-seen order.
fn scan_cart_console_icons() -> Vec<usize> {
    fn find_kzis(dir: &std::path::Path, depth: u32, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && depth > 0 {
                find_kzis(&p, depth - 1, out);
            } else if p.extension().and_then(|e| e.to_str()) == Some("kzi") {
                out.push(p);
            }
        }
    }
    let mut kzis = Vec::new();
    find_kzis(std::path::Path::new("/run/media"), 2, &mut kzis);
    find_kzis(std::path::Path::new("/media"), 2, &mut kzis);

    let mut icons: Vec<usize> = Vec::new();
    for kzi in kzis {
        let runtime = std::fs::read_to_string(&kzi)
            .ok()
            .and_then(|c| {
                c.lines()
                    .find(|l| l.starts_with("Runtime="))
                    .map(|l| l.trim_start_matches("Runtime=").trim().to_string())
            })
            .unwrap_or_default();
        for icon in console_icons_for_runtime(&runtime) {
            if !icons.contains(&icon) {
                icons.push(icon);
            }
        }
    }
    icons.truncate(6);
    icons
}

/// Anti-aliased white disc, tinted at draw time with the player color.
fn make_dot_texture() -> Texture2D {
    const SIZE: u16 = 64;
    let mut img = Image::gen_image_color(SIZE, SIZE, Color::new(1.0, 1.0, 1.0, 0.0));
    let c = (SIZE as f32 - 1.0) / 2.0;
    for py in 0..SIZE as u32 {
        for px in 0..SIZE as u32 {
            let dx = px as f32 - c;
            let dy = py as f32 - c;
            let d = (dx * dx + dy * dy).sqrt() / c;
            // Solid core, ~2px smooth rim.
            let a = ((0.95 - d) * 16.0).clamp(0.0, 1.0);
            img.set_pixel(px, py, Color::new(1.0, 1.0, 1.0, a));
        }
    }
    let tex = Texture2D::from_image(&img);
    tex.set_filter(FilterMode::Linear);
    tex
}

/// Map a pad's identity to a glyph set by name (physical device names from
/// InputPlumber, or gilrs names when unmanaged), falling back to vendor id.
fn legend_icon_for(vendor: Option<u16>, name: &str) -> LegendIcon {
    let n = name.to_lowercase();
    if n.contains("dualsense") || n.contains("sony") || n.contains("playstation") {
        return LegendIcon::PlayStation;
    }
    if n.contains("n64") || (n.contains("8bitdo") && n.contains("64")) {
        return LegendIcon::N64;
    }
    if n.contains("switch 2") {
        return LegendIcon::Switch2;
    }
    if n.contains("nintendo") || n.contains("switch") || n.contains("joy-con") || n.contains("pro controller") {
        return LegendIcon::Switch;
    }
    if n.contains("steam") {
        return LegendIcon::Steam;
    }
    match vendor {
        Some(0x054c) => LegendIcon::PlayStation,
        Some(0x057e) => LegendIcon::Switch,
        Some(0x28de) => LegendIcon::Steam,
        // 8BitDo's dongles present as XInput pads with 8BitDo's USB id.
        Some(0x2dc8) => LegendIcon::N64,
        _ => LegendIcon::Xbox,
    }
}

/// The glyph for the last-used pad. gilrs only sees InputPlumber's virtual
/// devices, so a "Microsoft X-Box 360 pad" (or anything InputPlumber-named)
/// may really be any brand — in that case ask InputPlumber for the physical
/// composite names and trust those instead.
fn pad_legend_icon(vendor: Option<u16>, name: &str) -> LegendIcon {
    let direct = legend_icon_for(vendor, name);
    // Only an Xbox verdict is ambiguous (it's the fallback and the virtual
    // pad brand) — anything else came from real identity, trust it.
    if direct == LegendIcon::Xbox {
        let composites = crate::pad_brand::current();
        // A Sony pad is never masked (the ds5 target keeps its identity), so
        // prefer the first composite that maps to something non-PlayStation.
        let mapped: Vec<LegendIcon> = composites
            .iter()
            .map(|c| legend_icon_for(None, c))
            .collect();
        if let Some(icon) = mapped
            .iter()
            .find(|i| **i != LegendIcon::PlayStation)
            .or_else(|| mapped.first())
        {
            return *icon;
        }
    }
    direct
}

/// Load cart-supplied art defensively: macroquad panics on unsupported
/// formats (e.g. a JPEG renamed .png), so verify the PNG signature first and
/// skip bad files instead of taking the whole bios down.
fn load_cart_texture(bytes: &[u8]) -> Option<Texture2D> {
    const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() > 8 && bytes[..8] == PNG_MAGIC {
        Some(Texture2D::from_file_with_format(bytes, Some(ImageFormat::Png)))
    } else {
        println!("[WARN] Cart art is not a real PNG (wrong extension?) - skipping it.");
        None
    }
}

/// Vertical alpha ramp (transparent top, opaque bottom), stretched by the GPU
/// wherever a gradient is needed — bilinear filtering interpolates it
/// perfectly smoothly, unlike stacked strips which band visibly.
fn make_fade_texture() -> Texture2D {
    const W: u16 = 4;
    const H: u16 = 128;
    let mut img = Image::gen_image_color(W, H, Color::new(1.0, 1.0, 1.0, 0.0));
    for y in 0..H as u32 {
        let a = y as f32 / (H as f32 - 1.0);
        for x in 0..W as u32 {
            img.set_pixel(x, y, Color::new(1.0, 1.0, 1.0, a));
        }
    }
    let tex = Texture2D::from_image(&img);
    tex.set_filter(FilterMode::Linear);
    tex
}

/// A heavily blurred copy of the cart's cover, for use as an ambient
/// background. Box-downsampling to a tiny image and letting the GPU stretch it
/// back with bilinear filtering IS the blur — the same trick the fade ramp
/// uses, and far cheaper than a shader pass.
fn make_blur_texture(bytes: &[u8]) -> Option<Texture2D> {
    const BW: usize = 32;
    const BH: usize = 18;
    let img = Image::from_file_with_format(bytes, Some(ImageFormat::Png)).ok()?;
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let mut out = Image::gen_image_color(BW as u16, BH as u16, WHITE);
    for by in 0..BH {
        for bx in 0..BW {
            let x0 = bx * w / BW;
            let x1 = (((bx + 1) * w / BW).max(x0 + 1)).min(w);
            let y0 = by * h / BH;
            let y1 = (((by + 1) * h / BH).max(y0 + 1)).min(h);
            let (mut r, mut g, mut b, mut n) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let c = img.get_pixel(x as u32, y as u32);
                    r += c.r;
                    g += c.g;
                    b += c.b;
                    n += 1.0;
                }
            }
            if n > 0.0 {
                out.set_pixel(bx as u32, by as u32, Color::new(r / n, g / n, b / n, 1.0));
            }
        }
    }
    let tex = Texture2D::from_image(&out);
    tex.set_filter(FilterMode::Linear);
    Some(tex)
}

/// Buckets per second in a track's loudness envelope.
const ENV_HZ: usize = 30;

/// Decode the cart's theme once on a worker thread and reduce it to an RMS
/// loudness envelope. Sampling that by playback position gives the dashboard
/// the beat for free — no realtime analysis, no audio-thread coupling.
fn spawn_bgm_envelope(path: PathBuf, slot: Arc<Mutex<Option<Vec<f32>>>>) {
    std::thread::spawn(move || {
        let Ok(bytes) = std::fs::read(&path) else { return };
        let Ok(decoder) = Decoder::new(Cursor::new(bytes)) else { return };
        let rate = decoder.sample_rate() as usize;
        let channels = decoder.channels() as usize;
        let per_bucket = (rate * channels / ENV_HZ).max(1);
        let mut env: Vec<f32> = Vec::new();
        let (mut acc, mut n) = (0.0f32, 0usize);
        for sample in decoder {
            acc += sample * sample;
            n += 1;
            if n >= per_bucket {
                env.push((acc / n as f32).sqrt());
                acc = 0.0;
                n = 0;
            }
        }
        // Normalise against the track's own peak, so a quiet theme reacts as
        // much as a loud one.
        let peak = env.iter().cloned().fold(0.0f32, f32::max);
        if peak > 0.0001 {
            for v in env.iter_mut() {
                *v /= peak;
            }
        }
        if let Ok(mut guard) = slot.lock() {
            *guard = Some(env);
        }
    });
}

/// Soft-focus disc texture for the bokeh motes: solid-ish core with a smooth
/// falloff to nothing at the rim, generated at startup so no asset is needed.
fn make_bokeh_texture() -> Texture2D {
    const SIZE: u16 = 64;
    let mut img = Image::gen_image_color(SIZE, SIZE, Color::new(0.0, 0.0, 0.0, 0.0));
    let c = (SIZE as f32 - 1.0) / 2.0;
    for py in 0..SIZE as u32 {
        for px in 0..SIZE as u32 {
            let dx = px as f32 - c;
            let dy = py as f32 - c;
            let d = (dx * dx + dy * dy).sqrt() / c;
            let a = (1.0 - d).clamp(0.0, 1.0).powf(1.6);
            img.set_pixel(px, py, Color::new(1.0, 1.0, 1.0, a));
        }
    }
    let tex = Texture2D::from_image(&img);
    tex.set_filter(FilterMode::Linear);
    tex
}

impl MetroState {
    pub fn new() -> Self {
        let badge_sd = Texture2D::from_file_with_format(include_bytes!("../../SDCARD.png"), None);
        badge_sd.set_filter(FilterMode::Linear);
        let badge_disc = Texture2D::from_file_with_format(include_bytes!("../../DISC.png"), None);
        badge_disc.set_filter(FilterMode::Linear);

        // Roll the mote field once; a fresh seed each boot so the sky is
        // never the same twice.
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(42);
        macroquad::rand::srand(seed);
        let bokeh = (0..BOKEH_COUNT)
            .map(|_| {
                use macroquad::rand::gen_range;
                Bokeh {
                    x: gen_range(0.0, 1.0),
                    y: gen_range(0.0, 1.0),
                    r: gen_range(10.0, 42.0),
                    speed: gen_range(0.006, 0.020),
                    wobble: gen_range(0.002, 0.012),
                    wobble_hz: gen_range(0.05, 0.20),
                    twinkle_hz: gen_range(0.04, 0.12),
                    phase: gen_range(0.0, std::f32::consts::TAU),
                    alpha: gen_range(0.04, 0.11),
                }
            })
            .collect();

        // Save icons for the Save Data marquee: every cached save's 32x32
        // icon, pixel-crisp. Missing/broken icons are simply skipped.
        let save_icons: Vec<Texture2D> = save::get_save_details("internal")
            .map(|details| {
                details
                    .into_iter()
                    .filter_map(|(_, _, icon)| {
                        let tex = load_cart_texture(&std::fs::read(icon).ok()?)?;
                        tex.set_filter(FilterMode::Nearest);
                        Some(tex)
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            tab: DEFAULT_TAB, prev_tab: DEFAULT_TAB, anim: 1.0, dir: 1.0,
            tile: primary_tile(DEFAULT_TAB),
            cover_tex: None, icon_tex: None, cart_label: None, cart_optical: false,
            cover_blur: None, cover_bg_vis: 0.0,
            bgm_env: Arc::new(Mutex::new(None)), bgm_start: 0.0, beat: 0.0,
            badge_sd, badge_disc, cover_key: String::new(),
            cart_vis: 0.0, outgoing: None,
            save_icons, cart_console_icons: Vec::new(),
            sel_anim: 1.0, prev_sel: None, press_flash: 0.0,
            intro_t: 0.0,
            outro_t: None,
            outro_action: OutroAction::Play,
            booting: false,
            legend_icon: LegendIcon::Keyboard,
            toasts: Vec::new(),
            had_cart: None,
            bokeh, bokeh_tex: make_bokeh_texture(), fade_tex: make_fade_texture(),
            bgm_path: None, bgm_sink: None, bgm_vol: 0.0,
            mounts_fp: String::new(), mounts_polled: -10.0,
        }
    }

    /// Restart the boot slide-in choreography (used when returning from the
    /// multicart selection screen).
    pub fn replay_intro(&mut self) {
        self.intro_t = 0.0;
    }

    /// Current confirm-glyph choice, for screens drawn outside metro::draw
    /// (the multicart selector shares the dashboard's device detection).
    pub fn legend_icon(&self) -> LegendIcon {
        self.legend_icon
    }

    pub fn stop_bgm(&mut self) {
        if let Some(sink) = self.bgm_sink.take() {
            sink.stop();
        }
        self.bgm_vol = 0.0;
    }

    fn go_tab(&mut self, to: usize, from_right: bool, land_on: usize) {
        self.prev_tab = self.tab;
        self.tab = to;
        self.tile = land_on;
        self.anim = 0.0;
        self.dir = if from_right { 1.0 } else { -1.0 };
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

fn ease_out_sine(t: f32) -> f32 {
    (t * std::f32::consts::FRAC_PI_2).sin()
}

/// Feathered drop shadow offset toward the lower right, faking dashx360's
/// 18px-blur DropShadow with expanding translucent rings.
fn draw_tile_shadow(x: f32, y: f32, w: f32, h: f32, s: f32, strength: f32) {
    let off = 3.0 * s * strength;
    const LAYERS: usize = 5;
    for i in 0..LAYERS {
        let spread = (i as f32 + 1.0) * 1.4 * s;
        let a = 0.16 * strength * (1.0 - i as f32 / LAYERS as f32);
        draw_rectangle(
            x - spread + off,
            y - spread + off,
            w + spread * 2.0,
            h + spread * 2.0,
            Color::new(0.0, 0.0, 0.0, a),
        );
    }
}

/// The "Booting Cartridge..." scene: a sweeping dotted ring over whatever
/// background the caller has drawn. Used by the dev-mode in-process boot
/// wait and by the production fade-out handoff to the session script.
pub fn draw_boot_screen(font_cache: &HashMap<String, Font>, config: &Config, s: f32) {
    let current_font = get_current_font(font_cache, config);
    let t = get_time() as f32;
    let (cx, cy) = (screen_width() / 2.0, screen_height() / 2.0 - 12.0 * s);
    let radius = 30.0 * s;
    const DOTS: usize = 12;
    let head = (t * 0.9).fract();
    TOAST_DOT.with(|dot| {
        for i in 0..DOTS {
            let frac = i as f32 / DOTS as f32;
            let ang = frac * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
            // How far this dot trails behind the sweeping head (0 = head).
            let d = (head - frac).rem_euclid(1.0);
            let a = (1.0 - d) * (1.0 - d) * 0.85 + 0.08;
            let dr = 3.4 * s * (0.75 + 0.45 * (1.0 - d));
            draw_texture_ex(
                dot,
                cx + ang.cos() * radius - dr,
                cy + ang.sin() * radius - dr,
                Color::new(1.0, 1.0, 1.0, a),
                DrawTextureParams {
                    dest_size: Some(vec2(dr * 2.0, dr * 2.0)),
                    ..Default::default()
                },
            );
        }
    });
    let label = "Booting Cartridge...";
    let label_size = (FONT_SIZE as f32 * s * 0.95) as u16;
    let dims = measure_text(label, Some(current_font), label_size, 1.0);
    let (tx, ty) = ((screen_width() - dims.width) / 2.0, cy + radius + 28.0 * s);
    let so = 1.0 * (label_size as f32 / FONT_SIZE as f32);
    draw_text_ex(label, tx + so, ty + so, TextParams {
        font: Some(current_font), font_size: label_size,
        color: Color::new(0.0, 0.0, 0.0, 0.8),
        ..Default::default()
    });
    draw_text_ex(label, tx, ty, TextParams {
        font: Some(current_font), font_size: label_size,
        color: Color::new(1.0, 1.0, 1.0, 0.9),
        ..Default::default()
    });
}

/// Two marquee rows of save icons drifting across the Save Data tile in
/// opposite directions, pixel-crisp and cropped cleanly at the tile edges.
fn draw_save_marquee(icons: &[Texture2D], x: f32, y: f32, w: f32, s: f32) {
    let t = get_time() as f32;
    let icon = 26.0 * s;
    let gap = 5.0 * s;
    let step = icon + gap;
    let n = icons.len();
    let span = step * n as f32;
    // Enough repetitions of the icon loop to always cover the tile width.
    let copies = ((w + step) / span).ceil() as usize + 1;

    // (row y offset, speed in design units/sec; sign = direction)
    for (row, (y_off, speed)) in [(9.0f32, -7.0f32), (43.0, 5.5)].into_iter().enumerate() {
        let py = y + y_off * s;
        let scroll = t * speed * s + row as f32 * step * 0.5;
        let o = scroll.rem_euclid(span);
        for j in 0..(n * copies) {
            let px = x + j as f32 * step - o;
            if px + icon <= x || px >= x + w {
                continue;
            }
            let tex = &icons[j % n];
            let (tw, th) = (tex.width(), tex.height());
            // Crop at the tile edges so partial icons never spill out.
            let mut dx = px;
            let mut dw = icon;
            let mut src_x = 0.0;
            if dx < x {
                let cut = x - dx;
                src_x = cut / icon * tw;
                dw -= cut;
                dx = x;
            }
            if dx + dw > x + w {
                dw = x + w - dx;
            }
            if dw <= 0.5 {
                continue;
            }
            draw_texture_ex(
                tex,
                dx,
                py,
                Color::new(1.0, 1.0, 1.0, 0.92),
                DrawTextureParams {
                    dest_size: Some(vec2(dw, icon)),
                    source: Some(Rect::new(src_x, 0.0, dw / icon * tw, th)),
                    ..Default::default()
                },
            );
        }
    }
}

/// Selection glow: a soft gradient halo radiating out from the tile edge
/// with a slow breathing pulse — replaces the old crisp blinking frame.
/// Layered outlines with squared falloff read as a smooth gradient once
/// they overlap (thickness 2x the layer step).
fn draw_focus_glow(x: f32, y: f32, w: f32, h: f32, s: f32, color: Color) {
    // Tuned against 80-unit dashboard tiles: ~9 units of halo, about 11% of
    // the tile's height.
    draw_focus_glow_ex(x, y, w, h, s, color, 1.0, 2.0);
}

/// Same glow with a tunable halo. Short elements (the settings rows, 18 units
/// tall) need a proportionally tighter spread or the rings from top and bottom
/// meet in the middle and the element reads as a solid colour wash.
fn draw_focus_glow_ex(x: f32, y: f32, w: f32, h: f32, s: f32, color: Color, spread: f32, core_th: f32) {
    let t = get_time() as f32;
    // Slow breath between 72% and 100% instead of a hard blink.
    let pulse = 0.72 + 0.28 * (0.5 + 0.5 * (t * 2.4).sin());
    const LAYERS: usize = 8;
    for i in (0..LAYERS).rev() {
        let off = (i as f32 + 1.0) * spread * s;
        let falloff = 1.0 - i as f32 / LAYERS as f32;
        let a = 0.32 * falloff * falloff * pulse;
        draw_rectangle_lines(
            x - off,
            y - off,
            w + off * 2.0,
            h + off * 2.0,
            2.0 * s,
            Color::new(color.r, color.g, color.b, a),
        );
    }
    // Crisp core edge so the selection still reads sharply.
    draw_rectangle_lines(x, y, w, h, core_th * s, Color::new(color.r, color.g, color.b, 0.95 * pulse));
}

/// Selection frame whose edge bars stop `r` short of the corners — reads as
/// dashx360's small corner radius at dash distance.
#[allow(dead_code)]
fn draw_focus_frame(x: f32, y: f32, w: f32, h: f32, th: f32, r: f32, color: Color) {
    draw_rectangle(x + r, y, w - 2.0 * r, th, color);
    draw_rectangle(x + r, y + h - th, w - 2.0 * r, th, color);
    draw_rectangle(x, y + r, th, h - 2.0 * r, color);
    draw_rectangle(x + w - th, y + r, th, h - 2.0 * r, color);
}

/// Tiles sit where their (col, row) says: columns run left to right, a column
/// containing the hero is banner-width, all others are one unit. Empty grid
/// slots are simply never declared, leaving a gap on purpose.
fn tile_rect(tile: &MetroTile, tiles: &[MetroTile], origin_x: f32, origin_y: f32, s: f32) -> Rect {
    // Sized in the app's 360p design space (scale_factor blows it up).
    // The hero is a wide banner matching the 920x430 cover art spec; small
    // tiles are landscape 185x131 like the real dash, with its thin gaps.
    let unit = 80.0 * s;
    let gap = 2.0 * s;
    let small_w = unit * (185.0 / 131.0);
    let hero_h = unit * 2.0 + gap;
    let hero_w = hero_h * (920.0 / 430.0);

    let mut x = origin_x;
    for c in 0..tile.col {
        let col_w = if tiles.iter().any(|t| t.col == c && t.hero) { hero_w } else { small_w };
        x += col_w + gap;
    }
    if tile.hero {
        Rect::new(x, origin_y, hero_w, hero_h)
    } else {
        Rect::new(x, origin_y + tile.row as f32 * (unit + gap), small_w, unit)
    }
}

/// The tile you reach moving one column left (dir = -1) or right (+1) of
/// `from`: nearest row wins, the hero matches any row, and columns whose
/// slots are all empty are stepped over.
fn spatial_neighbor(tiles: &[MetroTile], from: usize, dir: i32) -> Option<usize> {
    let cur = &tiles[from];
    let max_col = tiles.iter().map(|t| t.col).max().unwrap_or(0) as i32;
    let mut c = cur.col as i32 + dir;
    while (0..=max_col).contains(&c) {
        let mut best: Option<(usize, u8)> = None;
        for (i, t) in tiles.iter().enumerate() {
            if t.col as i32 != c {
                continue;
            }
            let d = if t.hero || cur.hero {
                0
            } else {
                (t.row as i8 - cur.row as i8).unsigned_abs()
            };
            if best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((i, d));
            }
        }
        if let Some((i, _)) = best {
            return Some(i);
        }
        c += dir;
    }
    None
}

// ===================================
// UPDATE
// ===================================

pub fn update(
    current_screen: &mut Screen,
    state: &mut MetroState,
    play_option_enabled: &mut bool,
    copy_logs_option_enabled: &mut bool,
    cart_connected: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    input_state: &mut InputState,
    sound_effects: &SoundEffects,
    config: &Config,
    log_messages: &std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    storage_state: &Arc<Mutex<StorageMediaState>>,
    fade_start_time: &mut Option<f64>,
    current_bgm: &mut Option<Sink>,
    music_cache: &HashMap<String, SamplesBuffer>,
    game_icon_queue: &mut Vec<(String, PathBuf)>,
    available_games: &mut Vec<(save::CartInfo, PathBuf)>,
    game_selection: &mut usize,
    flash_message: &mut Option<(String, f32)>,
    game_process: &mut Option<std::process::Child>,
) {
    *play_option_enabled = cart_connected.load(Ordering::Relaxed);
    *copy_logs_option_enabled = *play_option_enabled;

    // A cart that is gone takes its game list with it — otherwise a freshly
    // inserted cart inherits the previous cart's identity (stale
    // "Multi-Cart (N games)" after a swap).
    if !*play_option_enabled && !available_games.is_empty() {
        available_games.clear();
    }

    // Once a second, fingerprint the mounted media (name + mount time) so a
    // swap always changes the branding key even before any rescan happens.
    if get_time() - state.mounts_polled > 1.0 {
        state.mounts_polled = get_time();
        let mut fp = String::new();
        if let Ok(entries) = std::fs::read_dir("/run/media") {
            for e in entries.flatten() {
                fp.push_str(&e.file_name().to_string_lossy());
                if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                    if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                        fp.push_str(&format!(":{}", d.as_secs()));
                    }
                }
                fp.push(';');
            }
        }
        state.mounts_fp = fp;
    }

    // Refresh the Play hero's cart branding on insert/eject/swap.
    let cover_key = format!(
        "{}:{}:{}:{}",
        *play_option_enabled,
        state.mounts_fp,
        available_games.len(),
        available_games.first().map(|(_, p)| p.display().to_string()).unwrap_or_default(),
    );
    if cover_key != state.cover_key {
        state.cover_key = cover_key;
        let prev_cover = state.cover_tex.take();
        let prev_icon = state.icon_tex.take();
        let prev_label = state.cart_label.take();
        let prev_optical = state.cart_optical;
        state.cart_optical = false;
        state.bgm_path = None;
        state.cover_blur = None;
        state.stop_bgm();
        // The hover theme may have been ducking the system bgm when the cart
        // vanished — give the system its volume back.
        if let Some(system_bgm) = current_bgm.as_ref() {
            system_bgm.set_volume(1.0);
        }
        state.cart_console_icons = if *play_option_enabled {
            scan_cart_console_icons()
        } else {
            Vec::new()
        };
        if *play_option_enabled {
            let info = save::cart_display_info(available_games);
            state.cart_label = info.name;
            state.bgm_path = info.bgm;
            state.cart_optical = info.optical;
            let (cover, icon) = (info.cover, info.icon);
            if let Some(path) = cover {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Some(tex) = load_cart_texture(&bytes) {
                        tex.set_filter(FilterMode::Linear);
                        state.cover_tex = Some(tex);
                        state.cover_blur = make_blur_texture(&bytes);
                    }
                }
            }
            // Kick off the theme's loudness envelope for the beat reaction.
            state.bgm_env = Arc::new(Mutex::new(None));
            if let Some(bgm) = state.bgm_path.clone() {
                spawn_bgm_envelope(bgm, state.bgm_env.clone());
            }
            if let Some(path) = icon {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Some(tex) = load_cart_texture(&bytes) {
                        tex.set_filter(FilterMode::Nearest);
                        state.icon_tex = Some(tex);
                    }
                }
            }
        }
        // Animate only real identity changes (insert/eject/swap). The key
        // also churns when the game list rebuilds for the same cart, and
        // re-fading then would read as a glitch, so compare by name.
        if state.cart_label != prev_label {
            if prev_label.is_some() || prev_cover.is_some() {
                state.outgoing = Some(OutgoingBrand {
                    cover: prev_cover,
                    icon: prev_icon,
                    label: prev_label,
                    optical: prev_optical,
                    vis: state.cart_vis,
                });
            }
            state.cart_vis = 0.0;
        }
    }

    state.anim = (state.anim + get_frame_time() / SLIDE_TIME).min(1.0);

    // Focus/press animation clocks.
    state.sel_anim = (state.sel_anim + get_frame_time()).min(1.0);
    state.press_flash = (state.press_flash - get_frame_time() / PRESS_FLASH_TIME).max(0.0);
    state.intro_t = (state.intro_t + get_frame_time() / INTRO_TIME).min(1.0);

    // Legend glyph follows whatever device the user touched last.
    state.legend_icon = match input_state.last_source {
        InputSource::Keyboard => LegendIcon::Keyboard,
        InputSource::Pad => pad_legend_icon(input_state.pad_vendor, &input_state.pad_name),
    };

    // Player toasts: consume connect/disconnect events from the LED painter
    // and show them one at a time as a bottom pill.
    for ev in crate::pad_leds::take_pad_events() {
        let (r, g, b) = crate::pad_leds::PLAYER_COLORS[ev.slot.min(3)];
        sound_effects.play_toast(&config);
        state.toasts.push(Toast {
            text: format!(
                "Player {} {}",
                ev.slot + 1,
                if ev.connected { "Connected" } else { "Disconnected" }
            ),
            dot: Some(Color::from_rgba(r, g, b, 255)),
            icon: ToastIcon::Pad,
            t: 0.0,
        });
    }
    if let Some(toast) = state.toasts.first_mut() {
        toast.t += get_frame_time();
        if toast.t >= TOAST_TIME {
            state.toasts.remove(0);
        }
    }

    // Boot screen: the game owns the display; wait for its process to end,
    // then bring the dashboard back with the full intro.
    if state.booting {
        let done = match game_process.as_mut() {
            Some(child) => child.try_wait().map(|s| s.is_some()).unwrap_or(true),
            None => true,
        };
        if done {
            *game_process = None;
            state.booting = false;
            state.intro_t = 0.0;
        }
        return;
    }

    // Play outro: the dashboard slides back out (reverse intro), then the
    // launch/multicart handoff fires. Navigation is parked while it runs.
    if let Some(o) = state.outro_t {
        let o = o + get_frame_time() / OUTRO_TIME;
        if o < 1.0 {
            state.outro_t = Some(o);
        } else {
            state.outro_t = None;
            match state.outro_action {
                OutroAction::Play => {
                    activate_play(
                        current_screen, sound_effects, config, log_messages, fade_start_time,
                        current_bgm, music_cache, game_icon_queue, available_games,
                        game_selection, game_process,
                    );
                    if game_process.is_some() {
                        // A game actually spawned: hold on the boot screen
                        // instead of sliding the dash back in behind it.
                        state.booting = true;
                    } else {
                        // Multicart selector (or a failed launch) — the dash
                        // slides back in when it next shows.
                        state.intro_t = 0.0;
                    }
                }
                OutroAction::SaveData => {
                    state.intro_t = 0.0;
                    activate_save_data(
                        current_screen, input_state, storage_state, sound_effects, config,
                    );
                }
            }
        }
        return;
    }

    // Cart branding fade clocks: the current cart fades in, the parked
    // previous cart fades out and is dropped once invisible.
    state.cart_vis = (state.cart_vis + get_frame_time() / CART_ANIM_TIME).min(1.0);
    if let Some(out) = &mut state.outgoing {
        out.vis -= get_frame_time() / CART_ANIM_TIME;
        if out.vis <= 0.0 {
            state.outgoing = None;
        }
    }

    let tile_before = state.tile;
    let tab_before = state.tab;

    // A cart appearing takes you to it: jump to home, put the cursor on the
    // Play hero and say so. Suppressed on the first update so a cart already
    // inserted at boot doesn't announce itself.
    match state.had_cart {
        Some(false) if *play_option_enabled => {
            let hero = primary_tile(DEFAULT_TAB);
            if state.tab != DEFAULT_TAB {
                state.go_tab(DEFAULT_TAB, false, hero);
            } else {
                state.tile = hero;
            }
            state.toasts.push(Toast {
                text: "Cart Inserted".to_string(),
                dot: None,
                icon: ToastIcon::Cart,
                t: 0.0,
            });
            sound_effects.play_toast(&config);
        }
        _ => {}
    }
    state.had_cart = Some(*play_option_enabled);

    // prev/next (bumpers) hop a whole tab, landing on its primary tile.
    if input_state.prev && state.tab > 0 {
        let to = state.tab - 1;
        state.go_tab(to, false, primary_tile(to));
        sound_effects.play_cursor_move(&config);
    }
    if input_state.next && state.tab + 1 < TABS.len() {
        let to = state.tab + 1;
        state.go_tab(to, true, primary_tile(to));
        sound_effects.play_cursor_move(&config);
    }

    // Left/right walk columns spatially and cross pane edges like the real
    // dash: leaving left lands on the previous tab's rightmost tile, leaving
    // right lands on the next tab's leftmost.
    let tiles = TABS[state.tab].tiles;
    if input_state.left {
        if let Some(i) = spatial_neighbor(tiles, state.tile, -1) {
            state.tile = i;
            sound_effects.play_cursor_move(&config);
        } else if state.tab > 0 {
            let to = state.tab - 1;
            let land = TABS[to].tiles.iter().enumerate()
                .max_by_key(|(_, t)| (t.col, t.row == 0))
                .map(|(i, _)| i).unwrap_or(0);
            state.go_tab(to, false, land);
            sound_effects.play_cursor_move(&config);
        }
    }
    if input_state.right {
        if let Some(i) = spatial_neighbor(tiles, state.tile, 1) {
            state.tile = i;
            sound_effects.play_cursor_move(&config);
        } else if state.tab + 1 < TABS.len() {
            let to = state.tab + 1;
            let land = TABS[to].tiles.iter().enumerate()
                .min_by_key(|(_, t)| (t.col, t.row))
                .map(|(i, _)| i).unwrap_or(0);
            state.go_tab(to, true, land);
            sound_effects.play_cursor_move(&config);
        }
    }

    // Up/down toggle between the two rows of the current column (the hero
    // spans both rows, so it has no vertical neighbor).
    if input_state.up || input_state.down {
        let cur = &tiles[state.tile];
        if !cur.hero {
            let target_row = cur.row ^ 1;
            if let Some(i) = tiles.iter().position(|t| {
                !t.hero && t.col == cur.col && t.row == target_row
            }) {
                state.tile = i;
                sound_effects.play_cursor_move(&config);
            }
        }
    }

    // Focus-change bookkeeping: the newly focused tile grows in, the old one
    // shrinks back (same pane only — cross-tab, the slide hides it).
    if state.tile != tile_before || state.tab != tab_before {
        state.prev_sel = (state.tab == tab_before).then_some(tile_before);
        state.sel_anim = 0.0;
    }

    // --- Hover bgm: loop the cart's theme while the Play hero is selected ---
    let hero_hovered = *play_option_enabled
        && state.bgm_path.is_some()
        && TABS[state.tab]
            .tiles
            .get(state.tile)
            .map(|t| t.hero && t.action == BladeAction::Play)
            .unwrap_or(false);

    if hero_hovered && state.bgm_sink.is_none() {
        if let Some(path) = state.bgm_path.clone() {
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(decoder) = Decoder::new(Cursor::new(bytes)) {
                    let sink = Sink::connect_new(&AUDIO.stream.mixer());
                    sink.append(decoder.repeat_infinite());
                    sink.set_volume(0.0);
                    state.bgm_sink = Some(sink);
                    state.bgm_start = get_time();
                }
            }
        }
    }
    if state.bgm_sink.is_some() {
        let target = if hero_hovered { config.bgm_volume } else { 0.0 };
        let step = get_frame_time() / BGM_FADE_TIME * config.bgm_volume.max(0.05);
        if state.bgm_vol < target {
            state.bgm_vol = (state.bgm_vol + step).min(target);
        } else if state.bgm_vol > target {
            state.bgm_vol = (state.bgm_vol - step).max(target);
        }
        if let Some(sink) = &state.bgm_sink {
            sink.set_volume(state.bgm_vol);
        }
        // Fade the system bgm fully out underneath the cart's hover theme
        // (and back in as the hover theme fades away).
        if let Some(system_bgm) = current_bgm.as_ref() {
            let hover_fraction = (state.bgm_vol / config.bgm_volume.max(0.01)).min(1.0);
            system_bgm.set_volume(1.0 - hover_fraction);
        }
        if !hero_hovered && state.bgm_vol <= 0.001 {
            state.stop_bgm();
            if let Some(system_bgm) = current_bgm.as_ref() {
                system_bgm.set_volume(1.0);
            }
        }
    }

    // Eject: North on the Play hero cleanly unmounts the cart (and pops the
    // tray for discs). The branding fade-out happens on its own once the
    // mount disappears.
    if input_state.tertiary && *play_option_enabled {
        let on_play_hero = TABS[state.tab]
            .tiles
            .get(state.tile)
            .map(|t| t.hero && t.action == BladeAction::Play)
            .unwrap_or(false);
        if on_play_hero {
            state.stop_bgm();
            if let Some(system_bgm) = current_bgm.as_ref() {
                system_bgm.set_volume(1.0);
            }
            let _ = std::process::Command::new("sudo")
                .args(["-n", "/usr/bin/kazeta-eject"])
                .spawn();
            state.toasts.push(Toast {
                text: "Cart Ejected - Safe to Remove".to_string(),
                dot: None,
                icon: ToastIcon::Cart,
                t: 0.0,
            });
            sound_effects.play_back(&config);
        }
    }

    // Blurred cover crossfades in over the theme background while the Play
    // hero holds the cursor, and back out when it doesn't.
    let hero_focused = *play_option_enabled
        && TABS[state.tab]
            .tiles
            .get(state.tile)
            .map(|t| t.hero && t.action == BladeAction::Play)
            .unwrap_or(false);
    let bg_target = if hero_focused && state.cover_blur.is_some() { 1.0 } else { 0.0 };
    let bg_step = get_frame_time() / 0.45;
    if state.cover_bg_vis < bg_target {
        state.cover_bg_vis = (state.cover_bg_vis + bg_step).min(bg_target);
    } else if state.cover_bg_vis > bg_target {
        state.cover_bg_vis = (state.cover_bg_vis - bg_step).max(bg_target);
    }

    // Beat: sample the theme's loudness envelope at the current playback
    // position. Only while the hero is hovered and its theme is actually
    // playing — this is the cart's music, not the room's.
    // Onset, not loudness: how much louder this instant is than the last
    // second of the track. Mastered music holds a near-constant RMS, so a raw
    // level would just sit at an offset and never read as a beat — the ratio
    // against a moving baseline is what makes hits pop.
    let punch = if hero_hovered && state.bgm_sink.is_some() {
        state
            .bgm_env
            .try_lock()
            .ok()
            .and_then(|g| {
                g.as_ref().and_then(|env| {
                    if env.is_empty() {
                        return None;
                    }
                    let t = (get_time() - state.bgm_start).max(0.0);
                    let i = ((t * ENV_HZ as f64) as usize) % env.len();
                    let raw = env[i];
                    // Baseline over the preceding ~1s, wrapping with the loop.
                    let window = ENV_HZ.min(env.len());
                    let mut sum = 0.0;
                    for k in 0..window {
                        sum += env[(i + env.len() - k) % env.len()];
                    }
                    let baseline = (sum / window as f32).max(0.0001);
                    Some(((raw / baseline - 1.0) * 1.6).clamp(0.0, 1.0))
                })
            })
            .unwrap_or(0.0)
    } else {
        0.0
    };
    // Envelope follower: snap up on the hit, ease back down, so the room
    // punches rather than throbs.
    let dt = get_frame_time();
    let rate = if punch > state.beat { dt / 0.02 } else { dt / 0.18 };
    state.beat += (punch - state.beat) * rate.min(1.0);

    if input_state.select {
        // Metro press acknowledgment: the tile dips dark for a beat.
        state.press_flash = 1.0;
        // Leaving the menu (or launching) — cut the hover theme cleanly and
        // give the system bgm its volume back.
        state.stop_bgm();
        if let Some(system_bgm) = current_bgm.as_ref() {
            system_bgm.set_volume(1.0);
        }
        let tile = &TABS[state.tab].tiles[state.tile];
        match tile.action {
            BladeAction::SaveData => {
                // Departure animation first: the tile's save icons burst out
                // while the dash slides away, then the screen opens.
                state.outro_action = OutroAction::SaveData;
                state.outro_t = Some(0.0);
            }
            BladeAction::Play => {
                if *play_option_enabled {
                    // Reverse choreography first; activate_play fires when it
                    // lands. The pack's launch flourish (if any) starts with
                    // the outro so it plays over the slide-out.
                    sound_effects.play_launch(&config);
                    state.outro_action = OutroAction::Play;
                    state.outro_t = Some(0.0);
                } else {
                    sound_effects.play_reject(&config);
                }
            }
            BladeAction::CopyLogs => {
                if *copy_logs_option_enabled {
                    activate_copy_logs(flash_message, sound_effects, config);
                } else {
                    sound_effects.play_reject(&config);
                }
            }
            BladeAction::Wifi => { *current_screen = Screen::Wifi; sound_effects.play_select(&config); }
            BladeAction::Bluetooth => { *current_screen = Screen::Bluetooth; sound_effects.play_select(&config); }
            BladeAction::ThemeDownloader => { *current_screen = Screen::ThemeDownloader; sound_effects.play_select(&config); }
            BladeAction::RuntimeDownloader => { *current_screen = Screen::RuntimeDownloader; sound_effects.play_select(&config); }
            BladeAction::CdPlayer => { *current_screen = Screen::CdPlayer; sound_effects.play_select(&config); }
            BladeAction::UpdateChecker => { *current_screen = Screen::UpdateChecker; sound_effects.play_select(&config); }
            BladeAction::Settings => { *current_screen = Screen::GeneralSettings; sound_effects.play_select(&config); }
            BladeAction::About => { *current_screen = Screen::About; sound_effects.play_select(&config); }
        }
    }
}

// ===================================
// DRAW
// ===================================

/// One layer of Play-hero branding for the draw pass. During a swap two of
/// these exist at once: the ejected cart fading out under the new one fading
/// in.
struct HeroBrandDraw<'a> {
    cover: Option<&'a Texture2D>,
    icon: Option<&'a Texture2D>,
    badge: &'a Texture2D,
    label: Option<&'a str>,
    vis: f32, // 0..1 raw fade progress; eased and turned into alpha here
}

/// Draw one branding layer on the hero tile, faded to `vis` and settling
/// NXE-style from slightly small to full size. Two passes keep the paint
/// order identical to the static version: the cover goes under the selection
/// frame, the bar/badge/icon furniture goes over it.
fn draw_hero_brand(
    brand: &HeroBrandDraw,
    rx: f32, ry: f32, rw: f32, rh: f32,
    cover_pass: bool,
    motion: f32, // 0..1 hover blend for the cover's Ken Burns drift
    font_cache: &HashMap<String, Font>,
    config: &Config,
    s: f32,
) {
    let v = ease_out(brand.vis.clamp(0.0, 1.0));
    if v <= 0.003 {
        return;
    }
    let scale = 0.92 + 0.08 * v;
    let bx = rx + rw * (1.0 - scale) / 2.0;
    let by = ry + rh * (1.0 - scale) / 2.0;
    let bw = rw * scale;
    let bh = rh * scale;
    let tint = Color::new(1.0, 1.0, 1.0, v);

    if cover_pass {
        if let Some(tex) = brand.cover {
            // Ken Burns while hovered: a gentle push-in with a slow breathing
            // zoom and a lazy drift, done as a source-rect crop so the art
            // never spills outside the frame. motion 0 = the plain stretch.
            let src = if motion > 0.001 {
                let t = get_time() as f32;
                let tau = std::f32::consts::TAU;
                let (tw, th) = (tex.width(), tex.height());
                let zoom = 1.0 + motion * (0.05 + 0.03 * (0.5 - 0.5 * (t * tau / 14.0).cos()));
                let sw = tw / zoom;
                let sh = th / zoom;
                let px = 0.5 + 0.5 * motion * (t * tau / 23.0).sin();
                let py = 0.5 + 0.5 * motion * (t * tau / 31.0).cos();
                Some(Rect::new(
                    (tw - sw) * px.clamp(0.0, 1.0),
                    (th - sh) * py.clamp(0.0, 1.0),
                    sw,
                    sh,
                ))
            } else {
                None
            };
            draw_texture_ex(
                tex, bx, by, tint,
                DrawTextureParams { dest_size: Some(vec2(bw, bh)), source: src, ..Default::default() },
            );
        }
        return;
    }

    // Translucent "Play: NAME" bar along the bottom.
    let bar_h = 15.0 * s;
    draw_rectangle(bx, by + bh - bar_h, bw, bar_h, Color::new(0.0, 0.0, 0.0, 0.62 * v));
    let text = match brand.label {
        Some(name) => format!("Play: {}", name),
        None => "Play".to_string(),
    };
    // Hand-rolled shadowed text: text_with_color pins its shadow at 0.9
    // alpha, which would leave a black ghost of the label mid-fade.
    let font_size = (FONT_SIZE as f32 * s * 0.78) as u16;
    let font = get_current_font(font_cache, config);
    let shadow_offset = 1.0 * (font_size as f32 / FONT_SIZE as f32);
    let (tx, ty) = (bx + 5.0 * s, by + bh - 4.5 * s);
    draw_text_ex(&text, tx + shadow_offset, ty + shadow_offset, TextParams {
        font: Some(font),
        font_size,
        color: Color::new(0.0, 0.0, 0.0, 0.9 * v),
        ..Default::default()
    });
    draw_text_ex(&text, tx, ty, TextParams {
        font: Some(font),
        font_size,
        color: tint,
        ..Default::default()
    });
    // Media badge (SD card or disc art) top-right.
    let badge_size = 22.0 * s;
    draw_texture_ex(
        brand.badge,
        bx + bw - badge_size - 5.0 * s,
        by + 5.0 * s,
        tint,
        DrawTextureParams { dest_size: Some(vec2(badge_size, badge_size)), ..Default::default() },
    );
    // Cart icon: bottom-right, poking out over the top of the bar.
    if let Some(icon) = brand.icon {
        let icon_size = 26.0 * s;
        draw_texture_ex(
            icon,
            bx + bw - icon_size - 6.0 * s,
            by + bh - icon_size - 3.0 * s,
            tint,
            DrawTextureParams { dest_size: Some(vec2(icon_size, icon_size)), ..Default::default() },
        );
    }
}

/// Ambient bokeh motes over the background: slow upward drift, a lazy sway,
/// each breathing in and out on its own cycle. Positions are pure functions
/// of time, so this needs no mutable state.
fn draw_bokeh(state: &MetroState, intro: f32, s: f32) {
    let t = get_time() as f32;
    let w = screen_width();
    let h = screen_height();
    for b in &state.bokeh {
        let mut y = (b.y - t * b.speed).rem_euclid(1.15) - 0.075;
        let mut x = (b.x + (t * b.wobble_hz * std::f32::consts::TAU + b.phase).sin() * b.wobble)
            .rem_euclid(1.0);
        let breath = 0.5 - 0.5 * (t * b.twinkle_hz * std::f32::consts::TAU + b.phase * 1.7).cos();
        // The motes brighten and swell on the cart theme's beat while the
        // Play hero is hovered; `beat` is zero at every other moment.
        let mut a = b.alpha * breath * (1.0 + state.beat * 5.0);
        // Boot intro: every mote flies out of a point just below
        // bottom-center, curling as it travels to its resting spot.
        if intro < 1.0 {
            let dx = x - 0.5;
            let dy = y - 1.1;
            let spin = if b.phase < std::f32::consts::PI { 1.0 } else { -1.0 };
            let curl = (1.0 - intro) * (0.9 + b.phase * 0.25) * spin;
            let (sin_c, cos_c) = curl.sin_cos();
            x = 0.5 + (dx * cos_c - dy * sin_c) * intro;
            y = 1.1 + (dx * sin_c + dy * cos_c) * intro;
            a *= intro;
        }
        if a <= 0.003 {
            continue;
        }
        let r = b.r * s * (1.0 + state.beat * 0.55);
        draw_texture_ex(
            &state.bokeh_tex,
            x * w - r,
            y * h - r,
            // Faint green-white so the motes sit inside the dash's palette.
            Color::new(0.82, 1.0, 0.86, a),
            DrawTextureParams { dest_size: Some(vec2(r * 2.0, r * 2.0)), ..Default::default() },
        );
    }
}

fn draw_tab_pane(
    tab: &MetroTab,
    selected: Option<usize>,
    sel_anim: f32,
    prev_sel: Option<usize>,
    press_flash: f32,
    intro: f32,
    play_option_enabled: bool,
    copy_logs_option_enabled: bool,
    hero_brands: &[HeroBrandDraw],
    save_icons: &[Texture2D],
    cart_consoles: &[usize],
    hint_sd: &Texture2D,
    badge_disc: &Texture2D,
    fade_tex: &Texture2D,
    // Tab slide state: raw 0..1 progress, direction, and whether this pane
    // is the one on its way out. Columns move as a staggered wave.
    slide_anim: f32,
    slide_dir: f32,
    exiting: bool,
    beat: f32,
    origin_y: f32,
    _animation_state: &AnimationState,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    s: f32,
) {
    let origin_x = ORIGIN_X * s;
    let current_font = get_current_font(font_cache, config);

    // The shrinking-back tile only matters while this pane owns the cursor
    // and the remembered index is still in range.
    let prev_sel = if selected.is_some() {
        prev_sel.filter(|p| *p < tab.tiles.len() && Some(*p) != selected)
    } else {
        None
    };

    let draw_one = |idx: usize| {
        let tile = &tab.tiles[idx];
        let mut r = tile_rect(tile, tab.tiles, origin_x, origin_y, s);
        // Boot intro: tiles slide home from the screen edges — left half of
        // the pane from the left, right half from the right.
        if intro < 1.0 {
            let slide = (1.0 - intro) * screen_width() * 0.85;
            if r.x + r.w / 2.0 < screen_width() / 2.0 {
                r.x -= slide;
            } else {
                r.x += slide;
            }
        }
        // Tab switch: columns arrive (and leave) as a staggered wave instead
        // of the pane moving as one rigid block.
        if slide_anim < 1.0 {
            const STAGGER: f32 = 0.08; // per-column delay, in anim units
            let span = 1.0 - STAGGER * 3.0;
            let pt = ease_out(((slide_anim - STAGGER * tile.col as f32).clamp(0.0, span)) / span);
            let w = screen_width();
            r.x += if exiting {
                -slide_dir * pt * w
            } else {
                slide_dir * (1.0 - pt) * w
            };
        }
        let is_selected = selected == Some(idx);
        let is_disabled = match tile.action {
            BladeAction::Play => !play_option_enabled,
            BladeAction::CopyLogs => !copy_logs_option_enabled,
            _ => false,
        };

        // Flat Metro fill: Xbox green for flagged tiles, alternating slate
        // for the rest.
        let mut fill = if tile.green {
            XBOX_GREEN
        } else if idx % 2 == 0 {
            TILE_SLATE
        } else {
            TILE_SLATE_ALT
        };
        if is_disabled {
            fill = Color::new(fill.r * 0.45, fill.g * 0.45, fill.b * 0.45, 1.0);
        }

        // Focus scale, dashx360 timings: grow to 1.07x over 160ms (cubic
        // out), settle home over 120ms (sine out).
        let scale = if is_selected {
            1.0 + (SEL_SCALE - 1.0) * ease_out((sel_anim / SEL_GROW_TIME).min(1.0))
        } else if prev_sel == Some(idx) {
            1.0 + (SEL_SCALE - 1.0) * (1.0 - ease_out_sine((sel_anim / SEL_SHRINK_TIME).min(1.0)))
        } else {
            1.0
        };
        let rx = r.x - r.w * (scale - 1.0) / 2.0;
        let ry = r.y - r.h * (scale - 1.0) / 2.0;
        let rw = r.w * scale;
        let rh = r.h * scale;

        // A lifted tile casts a soft shadow onto the pane.
        let lift = ((scale - 1.0) / (SEL_SCALE - 1.0)).clamp(0.0, 1.0);
        if lift > 0.01 {
            draw_tile_shadow(rx, ry, rw, rh, s, lift);
        }

        let hero_play = tile.hero && tile.action == BladeAction::Play;

        draw_rectangle(rx, ry, rw, rh, fill);

        // Cover pass: cart covers sit under the lighting and frame. The
        // focus lift doubles as the Ken Burns hover blend.
        if hero_play {
            for brand in hero_brands {
                draw_hero_brand(brand, rx, ry, rw, rh, true, lift, font_cache, config, s);
            }
        }

        // Save Data wears a marquee of every saved game's icon, two rows
        // drifting in opposite directions.
        if tile.action == BladeAction::SaveData && !save_icons.is_empty() {
            draw_save_marquee(save_icons, rx, ry, rw, s * scale);
        }

        // Tile face icons, centered above the label zone.
        {
            // Tile art scales with the focus grow, so a hovered tile's icon
            // swells with it instead of sitting at a fixed size.
            let art = s * scale;
            let face: Option<(&Texture2D, f32)> = match tile.action {
                BladeAction::CdPlayer => Some((badge_disc, 44.0)),
                _ => None,
            };
            if let Some((tex, size)) = face {
                let d = size * art;
                draw_texture_ex(
                    tex,
                    rx + (rw - d) / 2.0,
                    ry + (rh - d) / 2.0 - 4.0 * s,
                    Color::new(1.0, 1.0, 1.0, 0.92),
                    DrawTextureParams { dest_size: Some(vec2(d, d)), ..Default::default() },
                );
            }
            let thread_face: Option<(&'static std::thread::LocalKey<Texture2D>, f32)> = match tile.action {
                BladeAction::Wifi => Some((&TILE_WIFI, 40.0)),
                BladeAction::Bluetooth => Some((&TILE_BLUETOOTH, 40.0)),
                BladeAction::Settings => Some((&TILE_SETTINGS, 42.0)),
                BladeAction::About => Some((&TILE_ABOUT, 40.0)),
                BladeAction::ThemeDownloader => Some((&TILE_THEMES, 40.0)),
                BladeAction::UpdateChecker => Some((&TILE_UPDATES, 40.0)),
                BladeAction::CopyLogs => Some((&TILE_LOGS, 40.0)),
                _ => None,
            };
            if let Some((key, size)) = thread_face {
                let d = size * art;
                key.with(|tex| {
                    draw_texture_ex(
                        tex,
                        rx + (rw - d) / 2.0,
                        ry + (rh - d) / 2.0 - 4.0 * s,
                        Color::new(1.0, 1.0, 1.0, 0.92),
                        DrawTextureParams { dest_size: Some(vec2(d, d)), ..Default::default() },
                    );
                });
            }
        }

        // Runtimes runs a slow carousel of console icons: three on the tile
        // at once, each drifting through its own lane before fading out and
        // being replaced by the next system in the pool. Positions and
        // alphas are pure functions of time, so this keeps no state.
        if tile.action == BladeAction::RuntimeDownloader {
            const SLOTS: usize = 4;
            const CYCLE: f32 = 5.4; // seconds one icon spends on the tile
            const FADE: f32 = 0.2;  // fraction of the cycle spent fading
            // Per-lane vertical offset so the four never sit in a straight row.
            const LANE_Y: [f32; SLOTS] = [-6.0, 5.0, -3.0, 7.0];
            let t = get_time() as f32;
            let art = s * scale;
            let cell = 22.0 * art;
            let lane_w = rw / SLOTS as f32;
            let zone_top = ry + 5.0 * art;
            let zone_h = rh * 0.60; // stays clear of the label band
            CONSOLE_ICONS.with(|icons| {
                for k in 0..SLOTS {
                    // Stagger the lanes so they never swap in unison.
                    let tk = t + k as f32 * CYCLE / SLOTS as f32;
                    let n = (tk / CYCLE).floor();
                    let p = tk / CYCLE - n;
                    let env = (p / FADE).min((1.0 - p) / FADE).min(1.0);
                    if env <= 0.01 {
                        continue;
                    }
                    // Consecutive pool entries, so the three on screen are
                    // always different systems.
                    let seq = (n as i64 * SLOTS as i64 + k as i64)
                        .rem_euclid(icons.len() as i64) as usize;
                    let h1 = hash01(seq as u32 * 2 + 1);
                    let h2 = hash01(seq as u32 * 2 + 7);
                    // Lazy drift across the lane over the icon's lifetime,
                    // with a per-appearance offset so it never repeats.
                    let dx = (h1 - 0.5) * 9.0 * art + (p - 0.5) * 7.0 * art;
                    let dy = (h2 - 0.5) * 6.0 * art - (p - 0.5) * 5.0 * art + LANE_Y[k] * art;
                    let size = cell * (0.86 + 0.14 * env);
                    let x = rx + lane_w * (k as f32 + 0.5) - size / 2.0 + dx;
                    let y = zone_top + (zone_h - size) / 2.0 + dy;
                    draw_texture_ex(
                        &icons[seq],
                        x,
                        y,
                        Color::new(1.0, 1.0, 1.0, 0.92 * env),
                        DrawTextureParams {
                            dest_size: Some(vec2(size, size)),
                            ..Default::default()
                        },
                    );
                }
            });
        }

        // Console family icons for the cart's runtime(s) drift under the
        // hero, each sealed in a soap bubble that bobs on its own clock.
        // Anchored to the unscaled rect so the focus grow doesn't jiggle them.
        if hero_play && !hero_brands.is_empty() && !cart_consoles.is_empty() {
            let t = get_time() as f32;
            let bub = 44.0 * s;   // bubble diameter
            let ico = 27.0 * s;   // console icon inside it
            let step = bub + 6.0 * s;
            let base_x = r.x + 4.0 * s;
            let base_y = r.y + r.h + 3.0 * s;
            CONSOLE_ICONS.with(|icons| {
                BUBBLE.with(|bubble| {
                    for (i, idx) in cart_consoles.iter().enumerate() {
                        let ph = i as f32 * 1.7;
                        // Lazy bob with a gentler sideways sway, each bubble
                        // on its own phase so they never move in lockstep.
                        let bx = base_x + i as f32 * step
                            + (t * 0.55 + ph * 1.3).sin() * 2.5 * s;
                        // Bubbles ride the beat too, bobbing higher on hits.
                        let by = base_y + (t * 0.85 + ph).sin() * 3.5 * s - beat * 9.0 * s;
                        let tex = &icons[*idx];
                        let iw = ico * tex.width() / tex.height();
                        draw_texture_ex(
                            tex,
                            bx + (bub - iw) / 2.0,
                            by + (bub - ico) / 2.0,
                            Color::new(1.0, 1.0, 1.0, 0.95),
                            DrawTextureParams {
                                dest_size: Some(vec2(iw, ico)),
                                ..Default::default()
                            },
                        );
                        // Bubble over the icon: light enough that the art
                        // reads through, with the rim selling the glass.
                        draw_texture_ex(
                            bubble,
                            bx,
                            by,
                            Color::new(1.0, 1.0, 1.0, 0.55),
                            DrawTextureParams {
                                dest_size: Some(vec2(bub, bub)),
                                ..Default::default()
                            },
                        );
                    }
                });
            });
        }

        // Top shimmer (dashx360's white-fade band) on the hero only — the
        // fade texture stretches into a perfectly smooth gradient.
        if tile.hero {
            draw_texture_ex(
                fade_tex, rx, ry,
                Color::new(1.0, 1.0, 1.0, 0.28),
                DrawTextureParams {
                    dest_size: Some(vec2(rw, rh * 0.16)),
                    flip_y: true, // ramp is bottom-heavy; shimmer wants top
                    ..Default::default()
                },
            );
        }
        // Darkness rising from the bottom (72% black at the edge) so labels
        // always sit on shadow. The branded Play hero skips it — its
        // translucent bar already owns that zone.
        if !(hero_play && !hero_brands.is_empty()) {
            let grad_h = rh * 0.30;
            draw_texture_ex(
                fade_tex, rx, ry + rh - grad_h,
                Color::new(0.0, 0.0, 0.0, 0.72),
                DrawTextureParams {
                    dest_size: Some(vec2(rw, grad_h)),
                    ..Default::default()
                },
            );
        }

        // Empty-cart coaching: the bare Play hero pulses an SD glyph and an
        // "Insert Cartridge" hint instead of sitting blank.
        if hero_play && hero_brands.is_empty() {
            let pulse = 0.30 + 0.14 * (get_time() as f32 * 2.0).sin();
            let icon_h = 34.0 * s * scale;
            let icon_w = icon_h * hint_sd.width() / hint_sd.height();
            let cx = rx + rw / 2.0;
            let cy = ry + rh / 2.0;
            draw_texture_ex(
                hint_sd,
                cx - icon_w / 2.0,
                cy - icon_h + 4.0 * s,
                Color::new(1.0, 1.0, 1.0, pulse),
                DrawTextureParams {
                    dest_size: Some(vec2(icon_w, icon_h)),
                    ..Default::default()
                },
            );
            let hint = "Insert Cartridge";
            let hint_size = (FONT_SIZE as f32 * s * 0.9) as u16;
            let dims = measure_text(hint, Some(current_font), hint_size, 1.0);
            draw_text_ex(hint, cx - dims.width / 2.0, cy + 18.0 * s, TextParams {
                font: Some(current_font),
                font_size: hint_size,
                color: Color::new(1.0, 1.0, 1.0, pulse + 0.1),
                ..Default::default()
            });
        }

        if is_selected {
            // Metro selection: gradient glow halo breathing around the tile.
            // On the Play hero the halo also swells with the cart theme's
            // beat, so the selection pulses with the music.
            let border = string_to_color(&config.cursor_color);
            let spread = if hero_play { 1.0 + beat * 1.1 } else { 1.0 };
            draw_focus_glow_ex(rx, ry, rw, rh, s, border, spread, 2.0 + beat * 0.7);
        }

        // The Play hero's mockup furniture — translucent "Play: NAME" bar,
        // media badge, cart icon — comes from the brand layers so it fades
        // with them. An ejected cart's layer still draws over the (now
        // disabled) tile on its way out.
        if hero_play && !hero_brands.is_empty() {
            for brand in hero_brands {
                draw_hero_brand(brand, rx, ry, rw, rh, false, 0.0, font_cache, config, s);
            }
        } else {
            // Metro labels: sentence case, bottom-left inside the tile.
            // Shrink to fit so long labels never spill past the tile edge.
            let mut font_size = (FONT_SIZE as f32 * s * if tile.hero { 1.2 } else { 0.85 }) as u16;
            let max_w = rw - 12.0 * s;
            let dims = measure_text(tile.label, Some(current_font), font_size, 1.0);
            if dims.width > max_w && dims.width > 0.0 {
                font_size = ((font_size as f32) * max_w / dims.width).floor() as u16;
            }
            let label_color = if is_disabled {
                Color::new(1.0, 1.0, 1.0, 0.45)
            } else {
                WHITE
            };
            text_with_color(
                font_cache, config, tile.label,
                rx + 6.0 * s,
                ry + rh - 6.0 * s,
                font_size, label_color,
            );
        }

        // Press acknowledgment: the tile dips dark for a beat after select.
        if is_selected && press_flash > 0.0 {
            draw_rectangle(rx, ry, rw, rh, Color::new(0.0, 0.0, 0.0, 0.18 * press_flash));
        }
    };

    // Z-order like the real dash: resting tiles first, then the tile
    // shrinking back, then the focused tile on top of everything — a grown
    // tile overlaps its neighbors instead of being cut by them.
    for idx in 0..tab.tiles.len() {
        if Some(idx) != selected && Some(idx) != prev_sel {
            draw_one(idx);
        }
    }
    if let Some(p) = prev_sel {
        draw_one(p);
    }
    if let Some(sel) = selected {
        draw_one(sel);
    }
}

/// Metro-styled multicart game selection: a grid of slate tiles built around
/// the carts' 32x32 icons (nearest-neighbor upscaled, pixel-crisp), cart name
/// as the header, selected game's name below the grid. Navigation stays the
/// 5-wide grid handled by the caller.
pub fn draw_game_selection(
    games: &[(save::CartInfo, PathBuf)],
    game_icon_cache: &HashMap<String, Texture2D>,
    placeholder: &Texture2D,
    selected_game: usize,
    cart_label: Option<&str>,
    legend_icon: LegendIcon,
    _animation_state: &AnimationState,
    background_cache: &HashMap<String, Texture2D>,
    video_cache: &mut HashMap<String, VideoPlayer>,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    background_state: &mut BackgroundState,
    s: f32,
) {
    draw_rectangle(0.0, 0.0, screen_width(), screen_height(), BG_FALLBACK);
    render_background(background_cache, video_cache, config, background_state);

    let current_font = get_current_font(font_cache, config);

    // --- Header: small "select game" eyebrow, cart name big underneath ---
    let eyebrow_size = (FONT_SIZE as f32 * s * 0.8) as u16;
    text_with_color(
        font_cache, config, "select game",
        75.0 * s, 42.0 * s, eyebrow_size,
        Color::new(1.0, 1.0, 1.0, 0.45),
    );
    let title_size = (FONT_SIZE as f32 * s * 1.45) as u16;
    text_with_color(
        font_cache, config, cart_label.unwrap_or("Cartridge"),
        75.0 * s, 68.0 * s, title_size, WHITE,
    );

    // --- Grid of icon tiles, 5 wide (matches the caller's navigation) ---
    let unit = 66.0 * s;
    let gap = 6.0 * s;
    let cols = 5usize;
    let rows = (games.len() + cols - 1) / cols;
    let grid_w = cols as f32 * unit + (cols - 1) as f32 * gap;
    let start_x = (screen_width() - grid_w) / 2.0;
    let start_y = 100.0 * s;

    for (i, (cart_info, _)) in games.iter().enumerate() {
        let col = (i % cols) as f32;
        let row = (i / cols) as f32;
        let is_selected = i == selected_game;

        let (mut rx, mut ry, mut rw, mut rh) = (
            start_x + col * (unit + gap),
            start_y + row * (unit + gap),
            unit,
            unit,
        );
        if is_selected {
            let grow = 4.0 * s;
            rx -= grow; ry -= grow; rw += grow * 2.0; rh += grow * 2.0;
        }

        // Slate tile with the dashboard's vertical sheen.
        let fill = if i % 2 == 0 { TILE_SLATE } else { TILE_SLATE_ALT };
        const STRIPS: usize = 6;
        let strip_h = rh / STRIPS as f32;
        for strip in 0..STRIPS {
            let t = strip as f32 / (STRIPS - 1) as f32;
            let b = 1.08 - 0.16 * t;
            draw_rectangle(
                rx, ry + strip as f32 * strip_h, rw, strip_h + 1.0,
                Color::new((fill.r * b).min(1.0), (fill.g * b).min(1.0), (fill.b * b).min(1.0), 1.0),
            );
        }

        // The 32x32 icon, upscaled pixel-crisp to fill most of the tile.
        let icon = game_icon_cache.get(&cart_info.id).unwrap_or(placeholder);
        icon.set_filter(FilterMode::Nearest);
        let icon_size = rw * 0.72;
        draw_texture_ex(
            icon,
            rx + (rw - icon_size) / 2.0,
            ry + (rh - icon_size) / 2.0,
            WHITE,
            DrawTextureParams { dest_size: Some(vec2(icon_size, icon_size)), ..Default::default() },
        );

        if is_selected {
            let border = string_to_color(&config.cursor_color);
            draw_focus_glow(rx, ry, rw, rh, s, border);
        }
    }

    // --- Selected game's name, big and centered under the grid ---
    if let Some((cart_info, _)) = games.get(selected_game) {
        let name = cart_info.name.as_deref().unwrap_or(&cart_info.id);
        let name_size = (FONT_SIZE as f32 * s * 1.25) as u16;
        let dims = measure_text(name, Some(current_font), name_size, 1.0);
        let name_y = start_y + rows as f32 * (unit + gap) + 34.0 * s;
        text_with_color(
            font_cache, config, name,
            (screen_width() - dims.width) / 2.0, name_y, name_size, WHITE,
        );
        if let Some(runtime) = cart_info.runtime.as_deref() {
            let sub_size = (FONT_SIZE as f32 * s * 0.7) as u16;
            let sub = format!("runtime: {}", runtime);
            let sub_dims = measure_text(&sub, Some(current_font), sub_size, 1.0);
            text_with_color(
                font_cache, config, &sub,
                (screen_width() - sub_dims.width) / 2.0, name_y + 18.0 * s, sub_size,
                Color::new(1.0, 1.0, 1.0, 0.4),
            );
        }
    }

    // --- Button legend, lower right like the dashboard: confirm boots the
    // cart, back returns to the dash. Glyphs match the last-used device. ---
    {
        let legend_size = (FONT_SIZE as f32 * s * 0.8) as u16;
        let icon_h = 18.0 * s;
        let cy = 324.0 * s;
        let icon_gap = 4.0 * s;
        let group_gap = 16.0 * s;
        let dim_white = Color::new(1.0, 1.0, 1.0, 0.75);

        let back_lbl = "Back";
        let back_dims = measure_text(back_lbl, Some(current_font), legend_size, 1.0);
        let back_text_x = screen_width() - 24.0 * s - back_dims.width;
        LEGEND_ICONS_BACK.with(|icons| {
            draw_texture_ex(
                &icons[legend_icon as usize],
                back_text_x - icon_gap - icon_h,
                cy - icon_h / 2.0,
                WHITE,
                DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() },
            );
        });
        text_with_color(
            font_cache, config, back_lbl,
            back_text_x, cy + back_dims.offset_y / 2.0, legend_size, dim_white,
        );

        let play_lbl = "Play";
        let play_dims = measure_text(play_lbl, Some(current_font), legend_size, 1.0);
        let play_text_x = back_text_x - icon_gap - icon_h - group_gap - play_dims.width;
        LEGEND_ICONS.with(|icons| {
            draw_texture_ex(
                &icons[legend_icon as usize],
                play_text_x - icon_gap - icon_h,
                cy - icon_h / 2.0,
                WHITE,
                DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() },
            );
        });
        text_with_color(
            font_cache, config, play_lbl,
            play_text_x, cy + play_dims.offset_y / 2.0, legend_size, dim_white,
        );
    }
}

pub fn draw(
    state: &MetroState,
    play_option_enabled: bool,
    copy_logs_option_enabled: bool,
    animation_state: &AnimationState,
    logo_cache: &HashMap<String, Texture2D>,
    background_cache: &HashMap<String, Texture2D>,
    video_cache: &mut HashMap<String, VideoPlayer>,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    background_state: &mut BackgroundState,
    battery_info: &Option<BatteryInfo>,
    current_time_str: &str,
    gcc_adapter_poll_rate: &Option<u32>,
    scale_factor: f32,
    flash_message: Option<&str>,
) {
    // Theme background first; flat Metro gray when the theme has none.
    draw_rectangle(0.0, 0.0, screen_width(), screen_height(), BG_FALLBACK);
    render_background(background_cache, video_cache, config, background_state);

    // The cart's cover, blurred, fading in over the theme background while
    // the Play hero is hovered — the room takes on the game's colours.
    if state.cover_bg_vis > 0.001 {
        if let Some(tex) = &state.cover_blur {
            let v = ease_out(state.cover_bg_vis);
            let (w, h) = (screen_width(), screen_height());
            draw_texture_ex(tex, 0.0, 0.0, Color::new(1.0, 1.0, 1.0, v), DrawTextureParams {
                dest_size: Some(vec2(w, h)), ..Default::default()
            });
            // Hold it back so tiles and text keep their contrast — but lift
            // the veil on each hit, so the whole room brightens to the beat.
            let dim = (0.45 - state.beat * 0.20).max(0.20);
            draw_rectangle(0.0, 0.0, w, h, Color::new(0.0, 0.0, 0.0, dim * v));
        }
    }

    let s = scale_factor;
    let current_font = get_current_font(font_cache, config);
    let origin_y = 112.0 * s;

    // Boot intro choreography: bokeh swirls out of the bottom, tiles slide
    // in from the edges, the tab strip drops in from the top, and the status
    // furniture (clock, poll rate, version, legend) fades up last. A Play
    // outro runs the same choreography in reverse.
    let (intro, overlay_a) = if let Some(o) = state.outro_t {
        let p = 1.0 - ease_out(o);
        (p, ((p - 0.35) / 0.5).clamp(0.0, 1.0))
    } else {
        (
            ease_out(state.intro_t),
            ease_out_sine(((state.intro_t - 0.35) / 0.5).clamp(0.0, 1.0)),
        )
    };

    // Ambient bokeh motes float over the background, under everything else.
    if config.background_particles == "ON" {
        draw_bokeh(state, intro, s);
    }

    // Boot screen: just the background, a sweeping dotted ring, and a label —
    // shown from game spawn until the game's process takes over / exits.
    // (Dev-mode path; production goes through Screen::FadingOut, which calls
    // draw_boot_screen directly.)
    if state.booting {
        draw_boot_screen(font_cache, config, s);
        return;
    }

    // --- Hero branding layers: ejected cart fading out under the current
    // cart fading in ---
    let mut hero_brands: Vec<HeroBrandDraw> = Vec::new();
    if let Some(out) = &state.outgoing {
        hero_brands.push(HeroBrandDraw {
            cover: out.cover.as_ref(),
            icon: out.icon.as_ref(),
            badge: if out.optical { &state.badge_disc } else { &state.badge_sd },
            label: out.label.as_deref(),
            vis: out.vis,
        });
    }
    if play_option_enabled {
        hero_brands.push(HeroBrandDraw {
            cover: state.cover_tex.as_ref(),
            icon: state.icon_tex.as_ref(),
            badge: if state.cart_optical { &state.badge_disc } else { &state.badge_sd },
            label: state.cart_label.as_deref(),
            vis: state.cart_vis,
        });
    }

    // --- Panes: the active pane's columns wave in over the previous one's
    // columns waving out ---
    if state.anim < 1.0 && state.prev_tab != state.tab {
        draw_tab_pane(
            &TABS[state.prev_tab], None, 1.0, None, 0.0, intro,
            play_option_enabled, copy_logs_option_enabled,
            &hero_brands, &state.save_icons, &state.cart_console_icons, &state.badge_sd, &state.badge_disc, &state.fade_tex,
            state.anim, state.dir, true, state.beat, origin_y, animation_state, font_cache, config, s,
        );
    }
    draw_tab_pane(
        &TABS[state.tab], Some(state.tile), state.sel_anim, state.prev_sel, state.press_flash, intro,
        play_option_enabled, copy_logs_option_enabled,
        &hero_brands, &state.save_icons, &state.cart_console_icons, &state.badge_sd, &state.badge_disc, &state.fade_tex,
        state.anim, state.dir, false, state.beat, origin_y, animation_state, font_cache, config, s,
    );

    // --- Button legend, lower right like the real dash; the confirm glyph
    // matches the last-used device (keyboard/pad brand). Fades up with the
    // rest of the status furniture (hand-rolled shadowed text so the fixed
    // 0.9-alpha helper shadow can't ghost during the fade) ---
    if overlay_a > 0.0 {
        let legend_size = (FONT_SIZE as f32 * s * 0.8) as u16;
        let label = "Select";
        let dims = measure_text(label, Some(current_font), legend_size, 1.0);
        let end_x = screen_width() - 24.0 * s;
        let text_x = end_x - dims.width;
        let cy = 324.0 * s;
        let icon_h = 18.0 * s;
        LEGEND_ICONS.with(|icons| {
            draw_texture_ex(
                &icons[state.legend_icon as usize],
                text_x - 4.0 * s - icon_h,
                cy - icon_h / 2.0,
                Color::new(1.0, 1.0, 1.0, overlay_a),
                DrawTextureParams {
                    dest_size: Some(vec2(icon_h, icon_h)),
                    ..Default::default()
                },
            );
        });
        let shadowed = |text: &str, x: f32, y: f32, size: u16, alpha: f32| {
            let so = 1.0 * (size as f32 / FONT_SIZE as f32);
            draw_text_ex(text, x + so, y + so, TextParams {
                font: Some(current_font), font_size: size,
                color: Color::new(0.0, 0.0, 0.0, 0.9 * overlay_a),
                ..Default::default()
            });
            draw_text_ex(text, x, y, TextParams {
                font: Some(current_font), font_size: size,
                color: Color::new(1.0, 1.0, 1.0, alpha * overlay_a),
                ..Default::default()
            });
        };
        shadowed(label, text_x, cy + dims.offset_y / 2.0, legend_size, 0.75);

        // Eject appears only while the Play hero holds a cart to eject.
        let on_play_hero = TABS[state.tab]
            .tiles
            .get(state.tile)
            .map(|t| t.hero && t.action == BladeAction::Play)
            .unwrap_or(false);
        if on_play_hero && play_option_enabled {
            let ej_lbl = "Eject";
            let ej_dims = measure_text(ej_lbl, Some(current_font), legend_size, 1.0);
            let ej_text_x = text_x - 4.0 * s - icon_h - 16.0 * s - ej_dims.width;
            LEGEND_ICONS_EJECT.with(|icons| {
                draw_texture_ex(
                    &icons[state.legend_icon as usize],
                    ej_text_x - 4.0 * s - icon_h,
                    cy - icon_h / 2.0,
                    Color::new(1.0, 1.0, 1.0, overlay_a),
                    DrawTextureParams {
                        dest_size: Some(vec2(icon_h, icon_h)),
                        ..Default::default()
                    },
                );
            });
            shadowed(ej_lbl, ej_text_x, cy + ej_dims.offset_y / 2.0, legend_size, 0.75);
        }
    }

    // --- Tab strip: every tab name in a row, active one big and white;
    // during the intro the whole strip slides down from above the screen ---
    let strip_y = 62.0 * s - (1.0 - intro) * 120.0 * s;
    let mut x = ORIGIN_X * s;
    for (i, tab) in TABS.iter().enumerate() {
        let is_active = i == state.tab;
        let font_size = (FONT_SIZE as f32 * s * if is_active { 1.45 } else { 0.95 }) as u16;
        let color = if is_active {
            WHITE
        } else {
            Color::new(1.0, 1.0, 1.0, 0.42)
        };
        text_with_color(font_cache, config, tab.title, x, strip_y, font_size, color);
        let dims = measure_text(tab.title, Some(current_font), font_size, 1.0);
        x += dims.width + 13.0 * s;
    }

    render_ui_overlay_alpha(logo_cache, font_cache, config, battery_info, current_time_str, gcc_adapter_poll_rate, scale_factor, overlay_a, true, false);

    // --- Save Data departure: the tile's marquee icons burst outward,
    // growing and fading, while the dash slides away underneath ---
    if let (Some(o), OutroAction::SaveData) = (state.outro_t, state.outro_action) {
        if !state.save_icons.is_empty() {
            let p = ease_out(o);
            let tile = &TABS[0].tiles[0];
            let r = tile_rect(tile, TABS[0].tiles, ORIGIN_X * s, origin_y, s);
            let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
            let n = state.save_icons.len();
            for (i, tex) in state.save_icons.iter().enumerate() {
                // Even radial fan with a slight per-icon spread so rings of
                // icons don't fly in lockstep.
                let ang = i as f32 / n as f32 * std::f32::consts::TAU + 0.6;
                let dist = p * (240.0 + (i % 3) as f32 * 70.0) * s;
                let x = cx + ang.cos() * dist;
                let y = cy + ang.sin() * dist * 0.8;
                let size = 26.0 * s * (1.0 + 1.8 * p);
                let a = (1.0 - p) * 0.95;
                if a <= 0.01 {
                    continue;
                }
                draw_texture_ex(
                    tex,
                    x - size / 2.0,
                    y - size / 2.0,
                    Color::new(1.0, 1.0, 1.0, a),
                    DrawTextureParams {
                        dest_size: Some(vec2(size, size)),
                        ..Default::default()
                    },
                );
            }
        }
    }

    // --- Player toast: "[glyph] [color dot] Player N Connected" pill that
    // slides up from the bottom edge and drops away after a beat ---
    if let Some(toast) = state.toasts.first() {
        let v_in = ease_out((toast.t / 0.25).min(1.0));
        let v_out = ease_out(((TOAST_TIME - toast.t) / 0.25).clamp(0.0, 1.0));
        let v = v_in.min(v_out);
        let size = (FONT_SIZE as f32 * s * 0.85) as u16;
        let dims = measure_text(&toast.text, Some(current_font), size, 1.0);
        let icon_h = 16.0 * s;
        let dot_r = 4.5 * s;
        let pad = 10.0 * s;
        let gap = 7.0 * s;
        let dot_w = if toast.dot.is_some() { dot_r * 2.0 + gap } else { 0.0 };
        let pill_w = pad + icon_h + gap + dot_w + dims.width + pad;
        let pill_h = 28.0 * s;
        let px = (screen_width() - pill_w) / 2.0;
        let py = screen_height() - 46.0 * s * v;
        draw_rectangle(px, py, pill_w, pill_h, Color::new(0.06, 0.06, 0.07, 0.85 * v));
        draw_rectangle_lines(px, py, pill_w, pill_h, 1.2 * s, Color::new(1.0, 1.0, 1.0, 0.18 * v));
        let mut x = px + pad;
        let cy = py + pill_h / 2.0;
        let icon_tint = Color::new(1.0, 1.0, 1.0, v);
        let icon_params = DrawTextureParams {
            dest_size: Some(vec2(icon_h, icon_h)),
            ..Default::default()
        };
        match toast.icon {
            ToastIcon::Pad => TOAST_PAD_ICON.with(|icon| {
                draw_texture_ex(icon, x, cy - icon_h / 2.0, icon_tint, icon_params.clone());
            }),
            ToastIcon::Cart => {
                draw_texture_ex(&state.badge_sd, x, cy - icon_h / 2.0, icon_tint, icon_params.clone());
            }
        }
        x += icon_h + gap;
        if let Some(color) = toast.dot {
            TOAST_DOT.with(|dot| {
                draw_texture_ex(
                    dot,
                    x,
                    cy - dot_r,
                    Color::new(color.r, color.g, color.b, v),
                    DrawTextureParams { dest_size: Some(vec2(dot_r * 2.0, dot_r * 2.0)), ..Default::default() },
                );
            });
            x += dot_r * 2.0 + gap;
        }
        draw_text_ex(&toast.text, x, cy + dims.offset_y / 2.0, TextParams {
            font: Some(current_font),
            font_size: size,
            color: Color::new(1.0, 1.0, 1.0, 0.95 * v),
            ..Default::default()
        });
    }

    // --- Flash message, same treatment as the other menus ---
    if let Some(message) = flash_message {
        let font_size = (FONT_SIZE as f32 * s) as u16;
        let dims = measure_text(message, Some(current_font), font_size, 1.0);
        let x = screen_width() / 2.0 - dims.width / 2.0;
        let y = screen_height() - (60.0 * s);

        draw_rectangle(
            x - (10.0 * s),
            y - dims.height,
            dims.width + (20.0 * s),
            dims.height + (10.0 * s),
            Color::new(0.0, 0.0, 0.0, 0.7),
        );
        text_with_config_color(font_cache, config, message, x, y, font_size);
    }
}

// ===================================
// GUIDE OVERLAY
// ===================================

pub enum GuideAction {
    None,
    KazetaHome,
    PowerOff,
}

pub struct GuideState {
    pub open: bool,
    pub selection: usize,
}

impl GuideState {
    pub fn new() -> Self {
        Self { open: false, selection: 0 }
    }
}

// ===================================
// METRO SETTINGS SCREEN
// ===================================
// A dashboard-styled face for the existing settings pages: a column of value
// rows on the left, a detail pane on the right, page names as a tab strip.
// `ui::settings::update()` is not touched — this is a different picture of
// the same state machine, so every binding keeps its exact meaning, and
// `get_settings_value` stays the single source of every value string.

/// Dynamic choice lists, for showing the focused row's position in its list
/// ("7 / 13") and its neighbours. Static lists (colours, speeds, resolutions,
/// timezones) come straight from settings.rs's own consts, so they are never
/// duplicated here.
pub struct SettingsChoices<'a> {
    pub themes: &'a HashMap<String, crate::theme::Theme>,
    pub sinks: &'a [crate::AudioSink],
    pub bgm: &'a [String],
    pub sfx_packs: &'a [String],
    pub logos: &'a [String],
    pub backgrounds: &'a [String],
    pub fonts: &'a [String],
    pub legend: LegendIcon,
}

#[derive(Clone, Copy, PartialEq)]
enum SWidget {
    Toggle,
    Percent,
    Step,
    Enum,
    Swatch,
    Aspect,
    Action,
    Jump(bool), // true = forward (later page)
}

/// Which control a row gets. Jump is detected from the VALUE ("->" / "<-"),
/// never the label: AUDIO_SETTINGS[4] reads "VIDEO SETTINGS" but actually
/// jumps to the General page, so trusting labels would ship that mislabel.
/// Mirrors the shape of settings.rs `get_settings_value`, never its content.
fn s_widget(page: usize, index: usize, value: &str) -> SWidget {
    if value == "->" {
        return SWidget::Jump(true);
    }
    if value == "<-" {
        return SWidget::Jump(false);
    }
    match (page, index) {
        (1, 0) => SWidget::Action,
        (1, 2) => SWidget::Aspect,
        (1, 5) | (2, 0) | (2, 1) | (2, 2) => SWidget::Percent,
        (1, 3) | (1, 6) | (1, 7) | (1, 8) => SWidget::Toggle,
        (3, 2) | (3, 3) => SWidget::Swatch,
        (3, 5) | (3, 6) | (3, 7) | (3, 8) => SWidget::Step,
        _ => SWidget::Enum,
    }
}

/// Metro's own display labels: Title Case, shortened, and correct. The
/// settings.rs arrays keep their exact strings and indices for LIST/BLADES.
fn s_label(page: usize, index: usize) -> &'static str {
    match (page, index) {
        (1, 0) => "Reset Settings",
        (1, 1) => "Resolution",
        (1, 2) => "Aspect Ratio",
        (1, 3) => "Splash Screen",
        (1, 4) => "Time Zone",
        (1, 5) => "Brightness",
        (1, 6) => "Wi-Fi",
        (1, 7) => "Bluetooth",
        (1, 8) => "Autoboot",
        (1, 9) => "Audio",
        (2, 0) => "Master Volume",
        (2, 1) => "Music Volume",
        (2, 2) => "Effects Volume",
        (2, 3) => "Audio Output",
        (2, 4) => "General",
        (2, 5) => "Interface",
        (3, 0) => "Theme",
        (3, 1) => "Menu Position",
        (3, 2) => "Font Color",
        (3, 3) => "Cursor Color",
        (3, 4) => "Cursor Style",
        (3, 5) => "Cursor Blink",
        (3, 6) => "Transitions",
        (3, 7) => "Background Scroll",
        (3, 8) => "Color Shift",
        (3, 9) => "Menu Style",
        (3, 10) => "Screensaver",
        (3, 11) => "Audio",
        (3, 12) => "Assets",
        (4, 0) => "Background Music",
        (4, 1) => "Sound Pack",
        (4, 2) => "Logo",
        (4, 3) => "Background",
        (4, 4) => "Font",
        (4, 5) => "Interface",
        _ => "",
    }
}

/// Bounded prettifier for known value strings. Deliberately a lookup and not
/// a heuristic: a PipeWire sink name or a font filename must pass through
/// byte-identical rather than get title-cased into nonsense.
fn s_value_case(v: &str) -> String {
    match v {
        "ON" => "On", "OFF" => "Off",
        "SLOW" => "Slow", "NORMAL" => "Normal", "FAST" => "Fast",
        "BOX" => "Box", "TEXT" => "Text", "CONFIRM" => "Confirm",
        "WHITE" => "White", "BLACK" => "Black", "PINK" => "Pink", "RED" => "Red",
        "ORANGE" => "Orange", "YELLOW" => "Yellow", "GREEN" => "Green",
        "BLUE" => "Blue", "PURPLE" => "Purple",
        "CENTER" => "Center", "TOPLEFT" => "Top Left", "TOPRIGHT" => "Top Right",
        "BOTTOMLEFT" => "Bottom Left", "BOTTOMRIGHT" => "Bottom Right",
        "LIST" => "List", "BLADES" => "Blades", "METRO" => "Metro",
        other => return other.to_string(),
    }
    .to_string()
}

/// One line of plain-language help per row. Empty means "nothing truthful to
/// say" — a stale description is worse than none.
fn s_help(page: usize, index: usize) -> &'static str {
    match (page, index) {
        (1, 0) => "Restores every default, including the menu style. A restart is required.",
        (1, 1) => "Screen resolution. Only sizes matching the aspect ratio are offered.",
        (1, 2) => "Screen shape. Changing it also picks the best matching resolution.",
        (1, 3) => "Play the boot video before the dashboard appears.",
        (1, 4) => "Clock offset from UTC, used by the clock and date in the corner.",
        (1, 5) => "Panel backlight. No effect on displays without backlight control.",
        (1, 6) => "Wireless networking. Turning it off disconnects any active network.",
        (1, 7) => "Bluetooth radio. Turning it off disconnects wireless controllers.",
        (1, 8) => "Boot the inserted cartridge instead of stopping at the dashboard.",
        (1, 9) => "Opens the Audio page. The shoulder buttons change page too.",
        (2, 0) => "System output volume. Affects everything the console plays.",
        (2, 1) => "Volume of the dashboard's background music.",
        (2, 2) => "Volume of menu sound effects.",
        (2, 3) => "Which audio device the console plays through.",
        (2, 4) => "Opens the General page. The shoulder buttons change page too.",
        (2, 5) => "Opens the Interface page. The shoulder buttons change page too.",
        (3, 0) => "Applies a whole look at once: sounds, music, logo, background, font, colors AND menu style. A theme that names no style returns you to the List menu.",
        (3, 1) => "Where the menu sits on screen. Also moves the clock and status text.",
        (3, 2) => "Color of menu text. The Metro dashboard always draws its labels white.",
        (3, 3) => "Color of the selection cursor, including Metro's focus glow.",
        (3, 4) => "Classic cursor shape. Metro uses its glow instead.",
        (3, 5) => "How fast the classic cursor blinks.",
        (3, 6) => "How fast menu transitions play.",
        (3, 7) => "How fast a background image scrolls.",
        (3, 8) => "How fast the background's color gradient drifts.",
        (3, 9) => "Which dashboard to use: the classic List, the 360-style Blades, or Metro.",
        (3, 10) => "Dim the screen after sitting idle. Any button wakes it up.",
        (3, 11) => "Opens the Audio page. The shoulder buttons change page too.",
        (3, 12) => "Opens the Assets page. The shoulder buttons change page too.",
        (4, 0) => "Music that loops on the dashboard.",
        (4, 1) => "Set of menu sound effects.",
        (4, 2) => "Logo shown on the dashboard.",
        (4, 3) => "Wallpaper behind every menu.",
        (4, 4) => "Typeface for all menu text.",
        (4, 5) => "Opens the Interface page. The shoulder buttons change page too.",
        _ => "",
    }
}

/// The focused row's full choice list plus where the current value sits in it.
/// Returns None when the row has no enumerable list, or when the current value
/// isn't found — the pane then shows the value alone, which is the safe way to
/// fail (it can never point at the wrong entry).
fn s_choices(
    page: usize,
    index: usize,
    config: &Config,
    ch: &SettingsChoices,
    current: &str,
) -> Option<(Vec<String>, usize)> {
    use crate::ui::settings as st;
    let list: Vec<String> = match (page, index) {
        (1, 1) => st::RESOLUTIONS
            .iter()
            .filter(|r| st::matches_aspect_ratio(r, &config.aspect_ratio))
            .map(|r| r.to_string())
            .collect(),
        (1, 2) => st::ASPECT_RATIOS.iter().map(|r| r.to_string()).collect(),
        (1, 4) => st::TIMEZONES.iter().map(|t| t.to_uppercase()).collect(),
        (2, 3) => ch.sinks.iter().map(|s| s.name.to_uppercase()).collect(),
        (3, 0) => {
            let mut names: Vec<String> = ch.themes.keys().cloned().collect();
            names.sort();
            names.iter().map(|n| n.replace('_', " ").to_uppercase()).collect()
        }
        (3, 1) => ["CENTER", "TOPLEFT", "TOPRIGHT", "BOTTOMLEFT", "BOTTOMRIGHT"]
            .iter().map(|p| p.to_string()).collect(),
        (3, 2) | (3, 3) => st::COLORS.iter().map(|c| c.to_string()).collect(),
        (3, 4) => st::CURSOR_STYLES.iter().map(|c| c.to_string()).collect(),
        (3, 5) | (3, 6) | (3, 7) | (3, 8) => st::SPEEDS.iter().map(|c| c.to_string()).collect(),
        (3, 9) => st::MENU_STYLES.iter().map(|m| m.to_string()).collect(),
        (3, 10) => st::SCREENSAVER_TIMEOUTS.iter().map(|m| m.to_string()).collect(),
        (4, 0) => ch.bgm.iter().map(|v| asset_display(v)).collect(),
        (4, 1) => ch.sfx_packs.iter().map(|v| v.replace('_', " ").to_uppercase()).collect(),
        (4, 2) => ch.logos.iter().map(|v| asset_display(v)).collect(),
        (4, 3) => ch.backgrounds.iter().map(|v| asset_display(v)).collect(),
        (4, 4) => ch.fonts.iter().map(|v| asset_display(v)).collect(),
        _ => return None,
    };
    let at = list.iter().position(|e| e == current)?;
    Some((list, at))
}

/// Same shaping settings.rs applies to asset names when it displays them.
fn asset_display(v: &str) -> String {
    crate::utils::trim_extension(v).replace('_', " ").to_uppercase()
}

// --- Save Data wall geometry. The classic screen is a 13x5 grid of bare
// 32px icons; Metro trades capacity for legibility with a 5x3 wall of named
// tiles. ui::data::update() reads these so its cursor math matches.
pub const SAVE_COLS: usize = 5;
pub const SAVE_ROWS: usize = 3;
const SAVE_GAP: f32 = 4.0;
const SAVE_TILE_H: f32 = 54.0;
const SAVE_GRID_TOP: f32 = 90.0;
const SAVE_INTRO_TIME: f32 = 0.55;
const SAVE_POP_TIME: f32 = 0.20;
const SAVE_MEDIA_TIME: f32 = 0.30;

/// types.rs keeps its dialog easing inline, so the save screen carries its own.
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Dialog option verbs ("COPY", "CANCEL") in Metro's sentence case. Separate
/// from s_value_case, which formats settings values — the two screens share no
/// vocabulary and coupling them would be a trap.
fn dlg_label(v: &str) -> String {
    match v {
        "OK" => return "OK".to_string(),
        _ => {}
    }
    let lower = v.to_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

const SET_INTRO_TIME: f32 = 0.45;
const SET_LIST_TOP: f32 = 84.0;
const SET_LIST_H: f32 = 178.0;
const SET_CHIP_Y: f32 = 268.0;
const SET_CHIP_H: f32 = 18.0;
const TILE_FOCUS: Color = Color::new(0.38, 0.39, 0.41, 1.0);
const TILE_RED: Color = Color::new(0.52, 0.11, 0.11, 1.0);
const TILE_RED_DIM: Color = Color::new(0.30, 0.16, 0.16, 1.0);

/// Animation clocks for the settings screen. Lives here rather than in
/// MetroState because the screen is drawn straight from settings.rs and has
/// no state of its own to thread through.
struct SettingsAnim {
    last_draw: f64,
    entering: bool,
    intro: f32,
    dir: f32,
    page: usize,
    sel: usize,
    prev_sel: Option<usize>,
    sel_anim: f32,
    val: String,
    val_key: (usize, usize),
    val_flash: f32,
    val_dir: f32,
    meter: f32,
}

impl SettingsAnim {
    fn new() -> Self {
        Self {
            last_draw: -10.0, entering: true, intro: 1.0, dir: 1.0,
            page: 0, sel: usize::MAX, prev_sel: None, sel_anim: 1.0,
            val: String::new(), val_key: (0, usize::MAX),
            val_flash: 0.0, val_dir: 1.0, meter: 0.0,
        }
    }
}

thread_local! {
    static SET_ANIM: RefCell<SettingsAnim> = RefCell::new(SettingsAnim::new());
}

pub fn draw_settings(
    page_number: usize,
    options: &[&str],
    logo_cache: &HashMap<String, Texture2D>,
    background_cache: &HashMap<String, Texture2D>,
    video_cache: &mut HashMap<String, VideoPlayer>,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    selection: usize,
    background_state: &mut BackgroundState,
    battery_info: &Option<BatteryInfo>,
    current_time_str: &str,
    gcc_adapter_poll_rate: &Option<u32>,
    s: f32,
    system_volume: f32,
    brightness: f32,
    choices: &SettingsChoices,
) {
    let font = get_current_font(font_cache, config);
    let w = screen_width();
    let h = screen_height();
    let w_du = w / s;

    // --- Frame, derived from the live screen so 4:3 (480 design units wide)
    // and 16:10 work as well as 16:9 ---
    let m = ORIGIN_X;
    let small_w = 80.0 * (185.0 / 131.0);
    let grid_right = m + 4.0 * small_w + 3.0 * 2.0; // the dash's own tile-grid edge
    let content_r = grid_right.min(w_du - m);
    let content_w = content_r - m;
    let col_gap = 8.0;
    let list_w = (2.0 * small_w + 2.0).min((content_w - col_gap) * 0.55);
    let pane_w = content_w - col_gap - list_w;
    let list_x = m;
    let pane_x = m + list_w + col_gap;

    // --- Split the page's options into value rows and page-jump chips ---
    let mut rows: Vec<(usize, String)> = Vec::new();
    let mut chips: Vec<(usize, bool)> = Vec::new(); // (option index, forward)
    for i in 0..options.len() {
        let v = crate::ui::settings::get_settings_value(page_number, i, config, system_volume, brightness);
        match s_widget(page_number, i, &v) {
            SWidget::Jump(fwd) => chips.push((i, fwd)),
            _ => rows.push((i, v)),
        }
    }
    let n = rows.len().max(1) as f32;
    let pitch = ((SET_LIST_H + 2.0) / n).clamp(15.0, 26.0);
    let row_h = pitch - 2.0;

    let cur_val = crate::ui::settings::get_settings_value(page_number, selection, config, system_volume, brightness);
    let cur_widget = s_widget(page_number, selection, &cur_val);
    let cur_choices = s_choices(page_number, selection, config, choices, &cur_val);

    // --- Clocks ---
    let (intro, dir, entering, sel_anim, prev_sel, val_flash, val_dir, meter_disp) =
        SET_ANIM.with(|cell| {
            let mut a = cell.borrow_mut();
            let now = get_time();
            let dt = get_frame_time();
            // A gap between draws means we arrived from another screen.
            let fresh = now - a.last_draw > 0.25;
            a.last_draw = now;

            if fresh {
                a.intro = 0.0;
                a.entering = true;
                a.dir = 1.0;
                a.page = page_number;
                a.sel = selection;
                a.prev_sel = None;
                a.sel_anim = 1.0;
                a.val.clear();
                a.val_flash = 0.0;
            } else if page_number != a.page {
                a.dir = if (page_number + 4 - a.page) % 4 == 1 { 1.0 } else { -1.0 };
                a.page = page_number;
                a.intro = 0.0;
                a.entering = false;
                a.prev_sel = None;
                a.sel = selection;
                a.sel_anim = 1.0;
            } else if selection != a.sel {
                a.prev_sel = Some(a.sel);
                a.sel = selection;
                a.sel_anim = 0.0;
            }

            // Value edits are detected by diffing the string, so the renderer
            // never needs to see InputState and update() stays untouched.
            if a.val_key == (page_number, selection) && cur_val != a.val && !a.val.is_empty() {
                a.val_flash = 1.0;
                a.val_dir = match (
                    a.val.trim_end_matches('%').parse::<f32>(),
                    cur_val.trim_end_matches('%').parse::<f32>(),
                ) {
                    (Ok(old), Ok(new)) if cur_val.ends_with('%') => {
                        if new >= old { 1.0 } else { -1.0 }
                    }
                    _ => 1.0,
                };
            }
            if a.val_key != (page_number, selection) {
                a.val_key = (page_number, selection);
            }
            a.val = cur_val.clone();

            a.intro = (a.intro + dt / SET_INTRO_TIME).min(1.0);
            a.sel_anim = (a.sel_anim + dt).min(1.0);
            a.val_flash = (a.val_flash - dt / 0.14).max(0.0);

            // Meters and toggles ease toward their value so a 10% step reads
            // as a sweep rather than a jump.
            let target = match cur_widget {
                SWidget::Percent => cur_val.trim_end_matches('%').parse::<f32>().unwrap_or(0.0) / 100.0,
                _ => 0.0,
            };
            let k = (dt / 0.12).min(1.0);
            a.meter += (target - a.meter) * k;

            (a.intro, a.dir, a.entering, a.sel_anim, a.prev_sel, a.val_flash, a.val_dir, a.meter)
        });

    let overlay_a = ease_out_sine(((intro - 0.18) / 0.25).clamp(0.0, 1.0));

    // --- Text helpers. Hand-rolled shadow: the shared helper pins its shadow
    // at 0.9 alpha, which ghosts while the screen fades in. ---
    let txt = |text: &str, x: f32, y: f32, size: u16, color: Color| {
        let so = 1.0 * (size as f32 / FONT_SIZE as f32);
        draw_text_ex(text, x + so, y + so, TextParams {
            font: Some(font), font_size: size,
            color: Color::new(0.0, 0.0, 0.0, 0.85 * color.a),
            ..Default::default()
        });
        draw_text_ex(text, x, y, TextParams {
            font: Some(font), font_size: size, color, ..Default::default()
        });
    };
    let fs = |k: f32| ((FONT_SIZE as f32 * s * k) as u16).max(9);
    // Shrink to fit, then truncate with an ellipsis if it still overflows at
    // the 9-device-pixel floor.
    let fit = |text: &str, k: f32, max_w: f32| -> (String, u16) {
        let mut size = fs(k);
        let d = measure_text(text, Some(font), size, 1.0);
        if d.width > max_w && d.width > 0.0 {
            size = (((size as f32) * max_w / d.width).floor() as u16).max(9);
        }
        let mut out = text.to_string();
        if measure_text(&out, Some(font), size, 1.0).width > max_w {
            while out.chars().count() > 1
                && measure_text(&format!("{}…", out), Some(font), size, 1.0).width > max_w
            {
                out.pop();
            }
            out.push('…');
        }
        (out, size)
    };

    // --- Background ---
    draw_rectangle(0.0, 0.0, w, h, BG_FALLBACK);
    render_background(background_cache, video_cache, config, background_state);
    draw_rectangle(0.0, 0.0, w, h, Color::new(0.0, 0.0, 0.0, 0.35));
    // Top wash so the header reads over bright wallpapers (a stretched ramp,
    // never stacked strips — those band).
    FADE_TEX.with(|tex| {
        draw_texture_ex(tex, 0.0, 0.0, Color::new(0.0, 0.0, 0.0, 0.35), DrawTextureParams {
            dest_size: Some(vec2(w, 100.0 * s)),
            flip_y: true,
            ..Default::default()
        });
    });

    // --- Detail pane (drawn under the rows, so a focused row's glow bleeds
    // over its edge rather than being clipped by it) ---
    let pane_slide = if intro < 1.0 {
        let p = ease_out((intro / 0.34).min(1.0));
        if entering { (1.0 - p) * w * 0.60 } else { dir * (1.0 - p) * w * 0.60 }
    } else {
        0.0
    };
    let px = pane_x * s + pane_slide;
    let py = SET_LIST_TOP * s;
    let pw = pane_w * s;
    let ph = 132.0 * s;
    {
        let fill = match cur_widget {
            SWidget::Jump(_) => XBOX_GREEN,
            SWidget::Action => TILE_RED,
            _ => TILE_SLATE_ALT,
        };
        draw_rectangle(px, py, pw, ph, fill);
        FADE_TEX.with(|tex| {
            draw_texture_ex(tex, px, py, Color::new(1.0, 1.0, 1.0, 0.28), DrawTextureParams {
                dest_size: Some(vec2(pw, ph * 0.16)), flip_y: true, ..Default::default()
            });
            draw_texture_ex(tex, px, py + ph * 0.74, Color::new(0.0, 0.0, 0.0, 0.55), DrawTextureParams {
                dest_size: Some(vec2(pw, ph * 0.26)), ..Default::default()
            });
        });

        // Icon: what family of setting this is.
        let icon_d = 40.0 * s;
        let ix = px + 14.0 * s;
        let iy = py + 14.0 * s;
        let tint = Color::new(1.0, 1.0, 1.0, 0.92);
        let t = get_time() as f32;
        if let SWidget::Jump(fwd) = cur_widget {
            // Page chips get a big directional chevron rather than an icon.
            let cxx = ix + icon_d * 0.5;
            let cyy = iy + icon_d * 0.5;
            let hw = icon_d * 0.34;
            let hh = icon_d * 0.44;
            let tip = if fwd { cxx + hw } else { cxx - hw };
            let back = if fwd { cxx - hw * 0.45 } else { cxx + hw * 0.45 };
            draw_triangle(vec2(tip, cyy), vec2(back, cyy - hh), vec2(back, cyy + hh), tint);
        } else {
        match (page_number, selection) {
            (1, 6) => TILE_WIFI.with(|x| draw_texture_ex(x, ix, iy, tint,
                DrawTextureParams { dest_size: Some(vec2(icon_d, icon_d)), ..Default::default() })),
            (1, 7) => TILE_BLUETOOTH.with(|x| draw_texture_ex(x, ix, iy, tint,
                DrawTextureParams { dest_size: Some(vec2(icon_d, icon_d)), ..Default::default() })),
            (2, 0) | (2, 1) | (2, 2) | (2, 3) | (4, 0) | (4, 1) => TILE_DISC.with(|x| {
                // The music row's disc spins while a track is set.
                let spin = if page_number == 4 && cur_val != "OFF" { t * 0.25 } else { 0.0 };
                draw_texture_ex(x, ix, iy, tint, DrawTextureParams {
                    dest_size: Some(vec2(icon_d, icon_d)), rotation: spin, ..Default::default()
                })
            }),
            (4, 2) => {
                // Show the actual logo, so picking one isn't guesswork.
                // Logos are wide, so they get a banner-shaped slot.
                let drawn = logo_cache.get(&config.logo_selection).map(|tex| {
                    let max_w = 110.0 * s;
                    let ar = tex.height() / tex.width();
                    let mut lw = max_w;
                    let mut lh = lw * ar;
                    if lh > icon_d {
                        lh = icon_d;
                        lw = lh / ar;
                    }
                    draw_texture_ex(tex, ix, iy + (icon_d - lh) / 2.0, tint, DrawTextureParams {
                        dest_size: Some(vec2(lw, lh)), ..Default::default()
                    });
                });
                if drawn.is_none() {
                    // "None", or an asset that failed to load.
                    TILE_THEMES.with(|x| draw_texture_ex(x, ix, iy,
                        Color::new(1.0, 1.0, 1.0, 0.45),
                        DrawTextureParams { dest_size: Some(vec2(icon_d, icon_d)), ..Default::default() }));
                }
            }
            (3, _) | (4, _) => TILE_THEMES.with(|x| {
                draw_texture_ex(x, ix, iy, tint, DrawTextureParams {
                    dest_size: Some(vec2(icon_d, icon_d)), ..Default::default()
                })
            }),
            (1, 2) => {
                // Aspect ratio draws its own true proportion instead of art.
                let (aw, ah) = match cur_val.as_str() {
                    "4:3" => (44.0, 33.0),
                    "16:10" => (44.0, 27.5),
                    _ => (44.0, 24.75),
                };
                draw_rectangle_lines(ix, iy + (40.0 - ah) * 0.5 * s, aw * s, ah * s, 1.5 * s,
                    Color::new(1.0, 1.0, 1.0, 0.7));
            }
            _ => TILE_SETTINGS.with(|x| draw_texture_ex(x, ix, iy, tint,
                DrawTextureParams { dest_size: Some(vec2(icon_d, icon_d)), ..Default::default() })),
        }
        }

        // Position in the choice list: dots when short, "7 / 25" when long.
        if let Some((list, at)) = &cur_choices {
            if list.len() > 1 {
                let cy = py + 34.0 * s;
                if list.len() <= 13 {
                    let gap = 6.0 * s;
                    let total = (list.len() - 1) as f32 * gap;
                    let start = px + pw - 14.0 * s - total;
                    TOAST_DOT.with(|dot| {
                        for i in 0..list.len() {
                            let r = if i == *at { 2.8 * s } else { 2.0 * s };
                            let a = if i == *at { 0.95 } else { 0.30 };
                            draw_texture_ex(dot, start + i as f32 * gap - r, cy - r,
                                Color::new(1.0, 1.0, 1.0, a),
                                DrawTextureParams { dest_size: Some(vec2(r * 2.0, r * 2.0)), ..Default::default() });
                        }
                    });
                } else {
                    let label = format!("{} / {}", at + 1, list.len());
                    let size = fs(0.72);
                    let d = measure_text(&label, Some(font), size, 1.0);
                    txt(&label, px + pw - 14.0 * s - d.width, cy + d.offset_y * 0.5, size,
                        Color::new(1.0, 1.0, 1.0, 0.55));
                }
            }
        }

        // Name of the focused setting.
        let (name, name_size) = fit(s_label(page_number, selection), 1.0, pw - 28.0 * s);
        txt(&name, px + 14.0 * s, py + 76.0 * s, name_size, Color::new(1.0, 1.0, 1.0, 0.85));

        // Widget band: the same control as the row, at pane scale.
        let band_y = py + 84.0 * s;
        match cur_widget {
            SWidget::Percent => {
                let cells_x = px + 14.0 * s;
                let cells_w = pw - 28.0 * s;
                let gap = 1.6 * s;
                let cw = (cells_w - 9.0 * gap) / 10.0;
                let filled = ((meter_disp * 10.0).round() as i32).clamp(0, 10) as usize;
                let fill_col = string_to_color(&config.cursor_color);
                for i in 0..10 {
                    let c = if i < filled { fill_col } else { Color::new(1.0, 1.0, 1.0, 0.16) };
                    draw_rectangle(cells_x + i as f32 * (cw + gap), band_y, cw, 9.0 * s, c);
                }
            }
            SWidget::Step => {
                let steps = ["OFF", "SLOW", "NORMAL", "FAST"];
                let filled = steps.iter().position(|v| *v == cur_val).unwrap_or(0);
                let cells_x = px + 14.0 * s;
                let gap = 8.0 * s;
                let cw = (pw - 28.0 * s - 2.0 * gap) / 3.0;
                let fill_col = string_to_color(&config.cursor_color);
                for i in 0..3 {
                    let c = if i < filled { fill_col } else { Color::new(1.0, 1.0, 1.0, 0.16) };
                    draw_rectangle(cells_x + i as f32 * (cw + gap), band_y, cw, 9.0 * s, c);
                }
            }
            SWidget::Toggle => {
                let on = cur_val == "ON";
                let tw = 60.0 * s;
                let th = 22.0 * s;
                let tx = px + 14.0 * s;
                let ty = py + 80.0 * s;
                draw_rectangle(tx, ty, tw, th,
                    if on { XBOX_GREEN } else { Color::new(1.0, 1.0, 1.0, 0.16) });
                draw_rectangle(if on { tx + tw - th } else { tx }, ty, th, th,
                    if on { Color::new(0.97, 0.97, 0.97, 1.0) } else { Color::new(1.0, 1.0, 1.0, 0.55) });
            }
            SWidget::Jump(_) => {
                let g = 22.0 * s;
                LEGEND_ICONS.with(|icons| {
                    draw_texture_ex(&icons[choices.legend as usize], px + 14.0 * s, band_y - 6.0 * s,
                        Color::new(1.0, 1.0, 1.0, 0.95),
                        DrawTextureParams { dest_size: Some(vec2(g, g)), ..Default::default() });
                });
                txt("Select", px + 14.0 * s + g + 6.0 * s, band_y + 10.0 * s, fs(0.8),
                    Color::new(1.0, 1.0, 1.0, 0.85));
            }
            SWidget::Enum | SWidget::Swatch | SWidget::Aspect => {
                // Neighbour ghosts: what's one press away in each direction.
                if let Some((list, at)) = &cur_choices {
                    if list.len() > 1 {
                        let ghost = Color::new(1.0, 1.0, 1.0, 0.30);
                        let chev = Color::new(1.0, 1.0, 1.0, 0.55);
                        let gy = py + 94.0 * s;
                        let max_w = (pw - 28.0 * s) * 0.40;
                        let nudge = (get_time() as f32 * 2.4).sin() * 1.0 * s;
                        let prev = &list[(at + list.len() - 1) % list.len()];
                        let next = &list[(at + 1) % list.len()];
                        let (pt, ps) = fit(&s_value_case(prev), 0.68, max_w);
                        let (nt, ns) = fit(&s_value_case(next), 0.68, max_w);
                        let lx = px + 14.0 * s;
                        let rx2 = px + pw - 14.0 * s;
                        draw_triangle(
                            vec2(lx - 3.0 * s + nudge, gy - 4.0 * s),
                            vec2(lx + 4.0 * s, gy - 8.0 * s),
                            vec2(lx + 4.0 * s, gy), chev);
                        draw_triangle(
                            vec2(rx2 + 3.0 * s - nudge, gy - 4.0 * s),
                            vec2(rx2 - 4.0 * s, gy - 8.0 * s),
                            vec2(rx2 - 4.0 * s, gy), chev);
                        txt(&pt, lx + 8.0 * s, gy, ps, ghost);
                        let nd = measure_text(&nt, Some(font), ns, 1.0);
                        txt(&nt, rx2 - 8.0 * s - nd.width, gy, ns, ghost);
                    }
                }
            }
            SWidget::Action => {}
        }

        // The value, big. This pane is the safety valve for long strings —
        // it always shows more than the row does.
        // A chip's raw value is the arrow glyph the classic screen shows —
        // say what it does instead.
        let shown = if matches!(cur_widget, SWidget::Jump(_)) {
            "Open".to_string()
        } else {
            s_value_case(&cur_val)
        };
        let kick = val_dir * 4.0 * s * val_flash;
        if page_number == 2 && selection == 3 {
            // Sink names are long and unshrinkable; give them two lines.
            let max_w = pw - 28.0 * s;
            let size = fs(0.85);
            let mut line1 = String::new();
            let mut line2 = String::new();
            for c in shown.chars() {
                let probe = format!("{}{}", line1, c);
                if line2.is_empty() && measure_text(&probe, Some(font), size, 1.0).width <= max_w {
                    line1.push(c);
                } else {
                    line2.push(c);
                }
            }
            let (l2, l2s) = fit(&line2, 0.85, max_w);
            txt(&line1, px + 14.0 * s + kick, py + 112.0 * s, size, WHITE);
            txt(&l2, px + 14.0 * s + kick, py + 126.0 * s, l2s, WHITE);
        } else {
            let (v, vs) = fit(&shown, 1.45, pw - 28.0 * s);
            txt(&v, px + 14.0 * s + kick, py + 122.0 * s, vs, WHITE);
        }
    }

    // --- Blurb strip ---
    {
        let bx = px;
        let by = 218.0 * s;
        let bw = pw;
        let bh = 68.0 * s;
        draw_rectangle(bx, by, bw, bh, TILE_SLATE);
        let help = s_help(page_number, selection);
        if !help.is_empty() {
            let a = (sel_anim / 0.12).min(1.0) * 0.72;
            let size = fs(0.72);
            let max_w = bw - 20.0 * s;
            // Greedy wrap, four lines max.
            let mut line = String::new();
            let mut lines: Vec<String> = Vec::new();
            for word in help.split_whitespace() {
                let probe = if line.is_empty() { word.to_string() } else { format!("{} {}", line, word) };
                if measure_text(&probe, Some(font), size, 1.0).width <= max_w {
                    line = probe;
                } else {
                    lines.push(std::mem::take(&mut line));
                    line = word.to_string();
                }
            }
            if !line.is_empty() {
                lines.push(line);
            }
            for (i, l) in lines.iter().take(4).enumerate() {
                txt(l, bx + 10.0 * s, by + (18.0 + i as f32 * 14.0) * s, size,
                    Color::new(1.0, 1.0, 1.0, a));
            }
        }
    }

    // --- Rows and chips, unfocused first then the focused one last, so a
    // grown element overlaps its neighbours instead of being cut by them ---
    let row_slide = |i: usize| -> f32 {
        if intro >= 1.0 {
            return 0.0;
        }
        let pt = ease_out(((intro - i as f32 * 0.018) / 0.30).clamp(0.0, 1.0));
        if entering { -(1.0 - pt) * w * 0.55 } else { dir * (1.0 - pt) * w * 0.55 }
    };

    let draw_row = |slot: usize, opt: usize, value: &String, focused: bool, shrinking: bool| {
        let k = if focused {
            ease_out((sel_anim / SEL_GROW_TIME).min(1.0))
        } else if shrinking {
            1.0 - ease_out_sine((sel_anim / SEL_SHRINK_TIME).min(1.0))
        } else {
            0.0
        };
        // A short bar can't take the tile's 1.07x scale (it would jump the
        // gutter), so focus lifts it by a fixed inset instead.
        let gx = k * 5.0 * s;
        let gy = k * ((pitch - row_h) * 0.5).min(2.0) * s;
        let rx = list_x * s - gx + row_slide(slot);
        let ry = (SET_LIST_TOP + slot as f32 * pitch) * s - gy;
        let rw = list_w * s + gx * 2.0;
        let rh = row_h * s + gy * 2.0;

        if k > 0.01 {
            draw_tile_shadow(rx, ry, rw, rh, s, k);
        }
        let fill = if page_number == 1 && opt == 0 {
            if focused { TILE_RED } else { TILE_RED_DIM }
        } else if focused {
            TILE_FOCUS
        } else if slot % 2 == 0 {
            TILE_SLATE
        } else {
            TILE_SLATE_ALT
        };
        draw_rectangle(rx, ry, rw, rh, fill);
        FADE_TEX.with(|tex| {
            draw_texture_ex(tex, rx, ry, Color::new(1.0, 1.0, 1.0, 0.05), DrawTextureParams {
                dest_size: Some(vec2(rw, rh * 0.40)), flip_y: true, ..Default::default()
            });
            draw_texture_ex(tex, rx, ry + rh * 0.60, Color::new(0.0, 0.0, 0.0, 0.40), DrawTextureParams {
                dest_size: Some(vec2(rw, rh * 0.40)), ..Default::default()
            });
        });

        let pad = 8.0 * s;
        let (label, lsize) = fit(s_label(page_number, opt), 0.80, list_w * s * 0.46);
        let ld = measure_text(&label, Some(font), lsize, 1.0);
        let base_y = ry + rh / 2.0 + ld.offset_y * 0.5;
        txt(&label, rx + pad, base_y, lsize,
            if focused { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.82) });

        // Value side. Every widget pins to the row's right edge so nothing
        // shifts as the value string changes width.
        let value_r = rx + rw - pad;
        let kick = if focused { val_dir * 4.0 * s * val_flash } else { 0.0 };
        let widget = s_widget(page_number, opt, value);
        match widget {
            SWidget::Toggle => {
                let on = value == "ON";
                let tw = 24.0 * s;
                let th = 10.0 * s;
                let ty = ry + rh / 2.0 - th / 2.0;
                let tx = value_r - tw;
                draw_rectangle(tx, ty, tw, th,
                    if on { XBOX_GREEN } else { Color::new(1.0, 1.0, 1.0, 0.16) });
                draw_rectangle(if on { tx + tw - th } else { tx }, ty, th, th,
                    if on { Color::new(0.97, 0.97, 0.97, 1.0) } else { Color::new(1.0, 1.0, 1.0, 0.55) });
                let word = if on { "On" } else { "Off" };
                let size = fs(0.72);
                let d = measure_text(word, Some(font), size, 1.0);
                let wx = tx - 8.0 * s - d.width;
                txt(word, wx + kick, base_y, size, Color::new(1.0, 1.0, 1.0, 0.9));
                // Radio rows also show their brand mark, lit or dim.
                let brand = match (page_number, opt) {
                    (1, 6) => Some(&TILE_WIFI),
                    (1, 7) => Some(&TILE_BLUETOOTH),
                    _ => None,
                };
                if let Some(key) = brand {
                    let g = 12.0 * s;
                    key.with(|tex| {
                        draw_texture_ex(tex, wx - 6.0 * s - g, ry + rh / 2.0 - g / 2.0,
                            Color::new(1.0, 1.0, 1.0, if on { 0.85 } else { 0.25 }),
                            DrawTextureParams { dest_size: Some(vec2(g, g)), ..Default::default() });
                    });
                }
            }
            SWidget::Percent => {
                let size = fs(0.72);
                let num_zone = 30.0 * s;
                let d = measure_text(value, Some(font), size, 1.0);
                txt(value, value_r - d.width + kick, base_y, size, Color::new(1.0, 1.0, 1.0, 0.9));
                let meter_r = value_r - num_zone - 6.0 * s;
                let meter_w = (list_w * 0.34).min(96.0) * s;
                let gap = 1.2 * s;
                let cw = (meter_w - 9.0 * gap) / 10.0;
                let ch = 5.0 * s;
                let cy = ry + rh / 2.0 - ch / 2.0;
                // One cell per 10%, which is exactly the step every handler
                // takes — so one press moves exactly one cell.
                let frac = if focused {
                    meter_disp
                } else {
                    value.trim_end_matches('%').parse::<f32>().unwrap_or(0.0) / 100.0
                };
                let filled = ((frac * 10.0).round() as i32).clamp(0, 10) as usize;
                let fill_col = if focused {
                    string_to_color(&config.cursor_color)
                } else {
                    Color::new(1.0, 1.0, 1.0, 0.92)
                };
                for i in 0..10 {
                    let c = if i < filled { fill_col } else { Color::new(1.0, 1.0, 1.0, 0.16) };
                    draw_rectangle(meter_r - meter_w + i as f32 * (cw + gap), cy, cw, ch, c);
                }
            }
            SWidget::Step => {
                let size = fs(0.72);
                let word_zone = 44.0 * s;
                let shown = s_value_case(value);
                let d = measure_text(&shown, Some(font), size, 1.0);
                txt(&shown, value_r - d.width + kick, base_y, size, Color::new(1.0, 1.0, 1.0, 0.9));
                let steps = ["OFF", "SLOW", "NORMAL", "FAST"];
                let filled = steps.iter().position(|v| *v == value.as_str()).unwrap_or(0);
                let gap = 3.0 * s;
                let cw = 10.0 * s;
                let ch = 5.0 * s;
                let cy = ry + rh / 2.0 - ch / 2.0;
                let cells_r = value_r - word_zone - 6.0 * s;
                let fill_col = if focused {
                    string_to_color(&config.cursor_color)
                } else {
                    Color::new(1.0, 1.0, 1.0, 0.92)
                };
                for i in 0..3 {
                    let c = if i < filled { fill_col } else { Color::new(1.0, 1.0, 1.0, 0.16) };
                    draw_rectangle(cells_r - 3.0 * (cw + gap) + i as f32 * (cw + gap), cy, cw, ch, c);
                }
            }
            SWidget::Action => {
                let g = 12.0 * s;
                LEGEND_ICONS.with(|icons| {
                    draw_texture_ex(&icons[choices.legend as usize], value_r - g, ry + rh / 2.0 - g / 2.0,
                        Color::new(1.0, 1.0, 1.0, 0.95),
                        DrawTextureParams { dest_size: Some(vec2(g, g)), ..Default::default() });
                });
                let size = fs(0.72);
                let d = measure_text("Confirm", Some(font), size, 1.0);
                txt("Confirm", value_r - g - 6.0 * s - d.width, base_y, size, Color::new(1.0, 1.0, 1.0, 0.9));
            }
            SWidget::Swatch | SWidget::Aspect | SWidget::Enum => {
                let shown = s_value_case(value);
                let extra = if matches!(widget, SWidget::Enum) { 0.0 } else { 16.0 * s };
                let (v, size) = fit(&shown, 0.76, list_w * s * 0.44 - extra);
                let d = measure_text(&v, Some(font), size, 1.0);
                let vx = value_r - d.width;
                txt(&v, vx + kick, base_y, size, Color::new(1.0, 1.0, 1.0, 0.92));
                match widget {
                    SWidget::Swatch => {
                        let sw = 10.0 * s;
                        let sx = vx - 6.0 * s - sw;
                        let sy = ry + rh / 2.0 - sw / 2.0;
                        draw_rectangle(sx, sy, sw, sw, string_to_color(value));
                        draw_rectangle_lines(sx, sy, sw, sw, 1.0 * s, Color::new(1.0, 1.0, 1.0, 0.30));
                    }
                    SWidget::Aspect => {
                        let (aw, ah) = match value.as_str() {
                            "4:3" => (18.0, 13.5),
                            "16:10" => (18.0, 11.25),
                            _ => (18.0, 10.125),
                        };
                        draw_rectangle_lines(vx - 6.0 * s - aw * s, ry + rh / 2.0 - ah * s / 2.0,
                            aw * s, ah * s, 1.0 * s, Color::new(1.0, 1.0, 1.0, 0.55));
                    }
                    _ => {}
                }
            }
            SWidget::Jump(_) => {}
        }

        if focused {
            if val_flash > 0.0 {
                draw_rectangle(rx, ry, rw, rh, Color::new(1.0, 1.0, 1.0, 0.14 * val_flash));
            }
            // Tighter halo than the dashboard's: an 18-unit bar needs the
            // same ~11% proportion the 80-unit tiles get.
            draw_focus_glow_ex(rx, ry, rw, rh, s, string_to_color(&config.cursor_color), 0.45, 1.0);
        }
    };

    let draw_chip = |slot: usize, opt: usize, forward: bool, focused: bool| {
        let k = if focused { ease_out((sel_anim / SEL_GROW_TIME).min(1.0)) } else { 0.0 };
        let g = k * 1.0 * s; // half the gutter, so two chips never overlap
        let slot_w = (list_w - 2.0) / 2.0;
        let bx = (list_x + if slot == 0 { 0.0 } else { slot_w + 2.0 }) * s;
        let rx = bx - g + row_slide(rows.len() + slot);
        let ry = SET_CHIP_Y * s - g;
        let rw = slot_w * s + g * 2.0;
        let rh = SET_CHIP_H * s + g * 2.0;

        if k > 0.01 {
            draw_tile_shadow(rx, ry, rw, rh, s, k);
        }
        draw_rectangle(rx, ry, rw, rh, if focused { XBOX_GREEN } else { TILE_SLATE_ALT });
        // Leading-edge bar on the side you're travelling toward.
        let bar_w = 3.0 * s;
        let bar_x = if forward { rx + rw - bar_w } else { rx };
        draw_rectangle(bar_x, ry, bar_w, rh, if focused { WHITE } else { XBOX_GREEN });

        let cy = ry + rh / 2.0;
        let chev_x = if forward { rx + rw - 12.0 * s } else { rx + 12.0 * s };
        let tipx = if forward { chev_x + 5.0 * s } else { chev_x - 5.0 * s };
        draw_triangle(
            vec2(tipx, cy),
            vec2(chev_x, cy - 4.5 * s),
            vec2(chev_x, cy + 4.5 * s),
            Color::new(1.0, 1.0, 1.0, 0.92));

        let (label, size) = fit(s_label(page_number, opt), 0.70, rw - 40.0 * s);
        let d = measure_text(&label, Some(font), size, 1.0);
        txt(&label, rx + (rw - d.width) / 2.0, cy + d.offset_y * 0.5, size,
            if focused { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.82) });

        if focused {
            let gi = 11.0 * s;
            let ix = if forward { rx + 6.0 * s } else { rx + rw - 6.0 * s - gi };
            LEGEND_ICONS.with(|icons| {
                draw_texture_ex(&icons[choices.legend as usize], ix, cy - gi / 2.0, WHITE,
                    DrawTextureParams { dest_size: Some(vec2(gi, gi)), ..Default::default() });
            });
            draw_focus_glow_ex(rx, ry, rw, rh, s, string_to_color(&config.cursor_color), 0.45, 1.0);
        }
    };

    // Pass 1: everything at rest. Pass 2: the tile shrinking back. Pass 3:
    // the focused element, on top.
    for (slot, (opt, value)) in rows.iter().enumerate() {
        if *opt != selection && Some(*opt) != prev_sel {
            draw_row(slot, *opt, value, false, false);
        }
    }
    // Back jumps take the left slot, forward jumps the right, so the chips
    // read in the same order as the pages they lead to.
    for (opt, forward) in chips.iter() {
        if *opt != selection {
            draw_chip(if *forward { 1 } else { 0 }, *opt, *forward, false);
        }
    }
    if let Some(p) = prev_sel {
        if let Some((slot, (opt, value))) = rows.iter().enumerate().find(|(_, (o, _))| *o == p) {
            draw_row(slot, *opt, value, false, true);
        }
    }
    if let Some((slot, (opt, value))) = rows.iter().enumerate().find(|(_, (o, _))| *o == selection) {
        draw_row(slot, *opt, value, true, false);
    }
    if let Some((opt, forward)) = chips.iter().find(|(o, _)| *o == selection) {
        draw_chip(if *forward { 1 } else { 0 }, *opt, *forward, true);
    }

    // --- Header: eyebrow, page tabs, bumper chevrons ---
    let strip_y = 62.0 * s - (1.0 - ease_out(intro)) * 120.0 * s;
    txt("settings", m * s, 36.0 * s - (1.0 - ease_out(intro)) * 120.0 * s, fs(0.80),
        Color::new(1.0, 1.0, 1.0, 0.45));
    {
        const PAGES: [&str; 4] = ["general", "audio", "interface", "assets"];
        // Measure first: a wide custom font could otherwise run the strip
        // into the clock.
        let mut strip_w = 0.0;
        for (i, name) in PAGES.iter().enumerate() {
            let size = fs(if i + 1 == page_number { 1.45 } else { 0.95 });
            strip_w += measure_text(name, Some(font), size, 1.0).width + 13.0 * s;
        }
        let squeeze = {
            let avail = w - (m + 110.0) * s;
            if strip_w > avail && strip_w > 0.0 { avail / strip_w } else { 1.0 }
        };
        let mut x = m * s;
        for (i, name) in PAGES.iter().enumerate() {
            let active = i + 1 == page_number;
            let size = ((fs(if active { 1.45 } else { 0.95 }) as f32 * squeeze) as u16).max(9);
            txt(name, x, strip_y, size,
                if active { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.42) });
            x += measure_text(name, Some(font), size, 1.0).width + 13.0 * s * squeeze;
        }
        let nudge = (get_time() as f32 * 2.4).sin() * 1.0 * s;
        let cy = strip_y - 6.0 * s;
        let chev = Color::new(1.0, 1.0, 1.0, 0.30);
        draw_triangle(
            vec2(m * s - 12.0 * s - nudge, cy),
            vec2(m * s - 6.0 * s, cy - 4.0 * s),
            vec2(m * s - 6.0 * s, cy + 4.0 * s), chev);
        draw_triangle(
            vec2(x + 6.0 * s + nudge, cy),
            vec2(x, cy - 4.0 * s),
            vec2(x, cy + 4.0 * s), chev);
    }

    // --- Legend, bottom right: what the buttons do here ---
    {
        let cy = 324.0 * s;
        let icon_h = 18.0 * s;
        let size = fs(0.80);
        let a = overlay_a;
        let leg = |text: &str, x: f32| {
            let d = measure_text(text, Some(font), size, 1.0);
            txt(text, x - d.width, cy + d.offset_y * 0.5, size, Color::new(1.0, 1.0, 1.0, 0.75 * a));
            x - d.width
        };
        let back_x = leg("Back", w - 24.0 * s);
        LEGEND_ICONS_BACK.with(|icons| {
            draw_texture_ex(&icons[choices.legend as usize], back_x - 4.0 * s - icon_h, cy - icon_h / 2.0,
                Color::new(1.0, 1.0, 1.0, a),
                DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() });
        });
        let group_x = back_x - 4.0 * s - icon_h - 16.0 * s;
        // Adjustable rows advertise left/right; everything else advertises
        // Select — and rows with nothing to change advertise neither.
        let adjustable = matches!(cur_widget,
            SWidget::Toggle | SWidget::Percent | SWidget::Step | SWidget::Enum
            | SWidget::Swatch | SWidget::Aspect)
            && cur_choices.as_ref().map(|(l, _)| l.len() > 1).unwrap_or(matches!(cur_widget,
                SWidget::Toggle | SWidget::Percent));
        if adjustable {
            let x = leg("Adjust", group_x);
            let bx = x - 4.0 * s - icon_h;
            let ccy = cy;
            let c = Color::new(1.0, 1.0, 1.0, 0.75 * a);
            let nudge = (get_time() as f32 * 2.4).sin() * 1.0 * s;
            draw_triangle(
                vec2(bx - nudge, ccy),
                vec2(bx + 6.0 * s, ccy - 5.0 * s),
                vec2(bx + 6.0 * s, ccy + 5.0 * s), c);
            draw_triangle(
                vec2(bx + icon_h + nudge, ccy),
                vec2(bx + icon_h - 6.0 * s, ccy - 5.0 * s),
                vec2(bx + icon_h - 6.0 * s, ccy + 5.0 * s), c);
        } else if matches!(cur_widget, SWidget::Action | SWidget::Jump(_)) {
            let x = leg("Select", group_x);
            LEGEND_ICONS.with(|icons| {
                draw_texture_ex(&icons[choices.legend as usize], x - 4.0 * s - icon_h, cy - icon_h / 2.0,
                    Color::new(1.0, 1.0, 1.0, a),
                    DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() });
            });
        }
    }

    // --- Bottom left: the shoulder buttons change page. Names follow the
    // controller brand rather than assuming Xbox. ---
    {
        let (lb, rb) = match choices.legend {
            LegendIcon::Keyboard => ("[", "]"),
            LegendIcon::PlayStation => ("L1", "R1"),
            LegendIcon::Switch | LegendIcon::Switch2 | LegendIcon::N64 => ("L", "R"),
            _ => ("LB", "RB"),
        };
        let a = overlay_a;
        let size = fs(0.62);
        let cw = 15.0 * s;
        let chh = 11.0 * s;
        let cy = 318.0 * s;
        for (i, name) in [lb, rb].iter().enumerate() {
            let bx = (m + i as f32 * 18.0) * s;
            draw_rectangle(bx, cy, cw, chh, Color::new(0.24, 0.25, 0.26, 0.85 * a));
            let d = measure_text(name, Some(font), size, 1.0);
            txt(name, bx + (cw - d.width) / 2.0, cy + chh / 2.0 + d.offset_y * 0.5, size,
                Color::new(1.0, 1.0, 1.0, 0.85 * a));
        }
        txt("Page", (m + 38.0) * s, cy + chh / 2.0 + 3.0 * s, fs(0.68),
            Color::new(1.0, 1.0, 1.0, 0.55 * a));
    }

    // The dashboard's own status furniture, minus the logo — a tall custom
    // logo would otherwise land on the detail pane.
    render_ui_overlay_alpha(logo_cache, font_cache, config, battery_info, current_time_str,
        gcc_adapter_poll_rate, s, overlay_a, true, true);
}

// ===================================
// METRO SAVE DATA SCREEN
// ===================================

/// Animation clocks for the save wall. Same shape as SettingsAnim: one
/// thread_local, ticked from the draw call, nothing threaded through main.rs.
struct SaveAnim {
    last_draw: f64,
    intro: f32,
    sel: usize,
    prev_sel: Option<usize>,
    sel_anim: f32,
    scroll: usize,
    media: usize,
    len: usize,
    pop: f32,
    pop_time: f32,
    ul_x: f32,
    ul_w: f32,
    veil: f32,
    legend: usize,
}

impl SaveAnim {
    fn new() -> Self {
        Self {
            last_draw: -10.0, intro: 1.0, sel: usize::MAX, prev_sel: None, sel_anim: 1.0,
            scroll: 0, media: usize::MAX, len: usize::MAX, pop: 1.0, pop_time: SAVE_POP_TIME,
            ul_x: 0.0, ul_w: 0.0, veil: 0.0, legend: 0,
        }
    }
}

thread_local! {
    static SAVE_ANIM: RefCell<SaveAnim> = RefCell::new(SaveAnim::new());
}

pub fn draw_save_data(
    state: &MetroState,
    selected_memory: usize,
    memories: &[Memory],
    icon_cache: &HashMap<String, Texture2D>,
    logo_cache: &HashMap<String, Texture2D>,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    storage_state: &Arc<Mutex<StorageMediaState>>,
    placeholder: &Texture2D,
    scroll_offset: usize,
    input_state: &InputState,
    animation_state: &AnimationState,
    playtime_cache: &mut PlaytimeCache,
    size_cache: &mut SizeCache,
    battery_info: &Option<BatteryInfo>,
    current_time_str: &str,
    gcc_adapter_poll_rate: &Option<u32>,
    dialog_state: &DialogState,
    s: f32,
) {
    let font = get_current_font(font_cache, config);
    let w = screen_width();
    let h = screen_height();
    let w_du = w / s;

    // Snapshot the storage list once and drop the guard — never hold a lock
    // across a draw, and `media` can legitimately be empty on a console with
    // no writable storage, so every later read goes through .get().
    let (media_ids, media_free, media_sel) = storage_state
        .lock()
        .map(|st| {
            (
                st.media.iter().map(|d| d.id.clone()).collect::<Vec<_>>(),
                st.media.iter().map(|d| d.free).collect::<Vec<_>>(),
                st.selected,
            )
        })
        .unwrap_or_default();

    // Same frame the settings screen uses, so the wall lines up with the
    // dashboard's tiles at every aspect ratio.
    let m = ORIGIN_X;
    let small_w = 80.0 * (185.0 / 131.0);
    let grid_right = m + 4.0 * small_w + 3.0 * 2.0;
    let content_r = grid_right.min(w_du - m);
    let content_w = content_r - m;
    let tile_w = (content_w - (SAVE_COLS as f32 - 1.0) * SAVE_GAP) / SAVE_COLS as f32;

    let grid_focus = input_state.ui_focus == UIFocus::Grid;
    let page_len = memories.len().saturating_sub(SAVE_COLS * scroll_offset);
    let on_page = page_len.min(SAVE_COLS * SAVE_ROWS);
    let rows_total = (memories.len() + SAVE_COLS - 1) / SAVE_COLS;

    let legend_now = match input_state.last_source {
        InputSource::Keyboard => LegendIcon::Keyboard,
        InputSource::Pad => pad_legend_icon(input_state.pad_vendor, &input_state.pad_name),
    };

    // --- Clocks: one borrow, tick everything, hand back plain values ---
    let (intro, sel_anim, prev_sel, pop, veil) = SAVE_ANIM.with(|cell| {
        let mut a = cell.borrow_mut();
        let now = get_time();
        let dt = get_frame_time();
        // Frame-relative, because this screen awaits a texture load per frame
        // while icons are still warming up; a fixed threshold would either
        // restart the cascade mid-flight or skip it entirely.
        let fresh = now - a.last_draw > (dt as f64 * 3.0).max(0.35);
        a.last_draw = now;

        if fresh {
            a.intro = 0.0;
            a.pop = 1.0;
            a.sel = selected_memory;
            a.prev_sel = None;
            a.sel_anim = 1.0;
            a.veil = 0.0;
            a.media = media_sel;
            a.len = memories.len();
            a.scroll = scroll_offset;
        }
        if selected_memory != a.sel {
            a.prev_sel = Some(a.sel);
            a.sel = selected_memory;
            a.sel_anim = 0.0;
        }
        if scroll_offset != a.scroll {
            a.scroll = scroll_offset;
            a.pop = 0.0;
            a.pop_time = SAVE_POP_TIME;
        }
        if memories.len() != a.len {
            a.len = memories.len();
            a.pop = 0.0;
            a.pop_time = SAVE_POP_TIME;
        }
        if media_sel != a.media {
            a.media = media_sel;
            a.pop = 0.0;
            a.pop_time = SAVE_MEDIA_TIME;
        }

        // Clamp dt so one long frame (an icon decode) can't eat the cascade.
        let step = dt.min(0.05);
        a.intro = (a.intro + step / SAVE_INTRO_TIME).min(1.0);
        a.sel_anim = (a.sel_anim + step).min(1.0);
        a.pop = (a.pop + step / a.pop_time).min(1.0);

        let d = match dialog_state {
            DialogState::Opening => smoothstep(animation_state.dialog_transition_progress),
            DialogState::Open => 1.0,
            DialogState::Closing => 1.0 - smoothstep(animation_state.dialog_transition_progress),
            DialogState::None => 0.0,
        };
        let target = 0.6 * d;
        a.veil += (target - a.veil) * (dt / 0.08).min(1.0);

        a.legend = legend_now as usize;
        (a.intro, a.sel_anim, a.prev_sel, a.pop, a.veil)
    });

    let overlay_a = ease_out_sine(((intro - 0.18) / 0.25).clamp(0.0, 1.0));
    let header_drop = (1.0 - ease_out(intro)) * 120.0 * s;

    // --- Text helpers (device pixels throughout) ---
    let txt = |text: &str, x: f32, y: f32, size: u16, color: Color| {
        let so = 1.0 * (size as f32 / FONT_SIZE as f32);
        draw_text_ex(text, x + so, y + so, TextParams {
            font: Some(font), font_size: size,
            color: Color::new(0.0, 0.0, 0.0, 0.85 * color.a), ..Default::default()
        });
        draw_text_ex(text, x, y, TextParams {
            font: Some(font), font_size: size, color, ..Default::default()
        });
    };
    let fs = |k: f32| ((FONT_SIZE as f32 * s * k) as u16).max(9);
    let fit = |text: &str, k: f32, max_w: f32| -> (String, u16) {
        let mut size = fs(k);
        let d = measure_text(text, Some(font), size, 1.0);
        if d.width > max_w && d.width > 0.0 {
            size = (((size as f32) * max_w / d.width).floor() as u16).max(9);
        }
        let mut out = text.to_string();
        if measure_text(&out, Some(font), size, 1.0).width > max_w {
            while out.chars().count() > 1
                && measure_text(&format!("{}…", out), Some(font), size, 1.0).width > max_w
            {
                out.pop();
            }
            out.push('…');
        }
        (out, size)
    };

    // --- Background continuity: main.rs already drew the theme background
    // immediately before this call, so we never redraw it. The bokeh keeps
    // drifting in the field the dashboard left it in. ---
    draw_rectangle(0.0, 0.0, w, h, Color::new(0.0, 0.0, 0.0, 0.35));
    FADE_TEX.with(|tex| {
        draw_texture_ex(tex, 0.0, 0.0, Color::new(0.0, 0.0, 0.0, 0.35), DrawTextureParams {
            dest_size: Some(vec2(w, 100.0 * s)), flip_y: true, ..Default::default()
        });
        draw_texture_ex(tex, 0.0, h - 70.0 * s, Color::new(0.0, 0.0, 0.0, 0.45), DrawTextureParams {
            dest_size: Some(vec2(w, 70.0 * s)), ..Default::default()
        });
    });
    if config.background_particles == "ON" {
        // The dashboard's outro collapsed the motes toward bottom-centre; feed
        // this screen's own intro so they fly back out instead of snapping.
        draw_bokeh(state, ease_out(intro), s);
    }

    // --- Header: eyebrow + storage tab strip ---
    txt("save data", m * s, 36.0 * s - header_drop, fs(0.80), Color::new(1.0, 1.0, 1.0, 0.45));
    let strip_y = 62.0 * s - header_drop;
    let shake = (animation_state.calculate_shake_offset(ShakeTarget::LeftArrow)
        + animation_state.calculate_shake_offset(ShakeTarget::RightArrow)) * s;
    let mut active_ul = (m * s, 0.0);
    if media_ids.is_empty() {
        txt("no storage", m * s + shake, strip_y, fs(1.45), Color::new(1.0, 1.0, 1.0, 0.35));
    } else {
        let names: Vec<String> = media_ids.iter().map(|id| id.to_lowercase().replace('_', " ")).collect();
        let mut strip_w = 0.0;
        for (i, n) in names.iter().enumerate() {
            let size = fs(if i == media_sel { 1.45 } else { 0.95 });
            strip_w += measure_text(n, Some(font), size, 1.0).width + 13.0 * s;
        }
        let avail = w - (m + 110.0) * s;
        let squeeze = if strip_w > avail && strip_w > 0.0 { avail / strip_w } else { 1.0 };
        let mut x = m * s + shake;
        for (i, n) in names.iter().enumerate() {
            let active = i == media_sel;
            let size = ((fs(if active { 1.45 } else { 0.95 }) as f32 * squeeze) as u16).max(9);
            let d = measure_text(n, Some(font), size, 1.0);
            if active {
                active_ul = (x, d.width);
                if !grid_focus {
                    // The strip has the cursor: back it with a plate and glow.
                    let px = x - 7.0 * s;
                    let py = strip_y - 16.0 * s;
                    let pw2 = d.width + 14.0 * s;
                    let ph2 = 24.0 * s;
                    draw_rectangle(px, py, pw2, ph2, Color::new(0.30, 0.31, 0.33, 0.55));
                    draw_focus_glow_ex(px, py, pw2, ph2, s,
                        string_to_color(&config.cursor_color), 0.45, 1.0);
                }
            }
            let col = if active {
                WHITE
            } else if grid_focus {
                Color::new(1.0, 1.0, 1.0, 0.42)
            } else {
                Color::new(1.0, 1.0, 1.0, 0.60)
            };
            txt(n, x, strip_y, size, col);
            x += d.width + 13.0 * s * squeeze;
        }
        if media_ids.len() > 1 {
            let nudge = (get_time() as f32 * 2.4).sin() * 1.0 * s;
            let cy = strip_y - 6.0 * s;
            let dim = |on: bool| if on { Color::new(1.0, 1.0, 1.0, 0.30) } else { Color::new(1.0, 1.0, 1.0, 0.10) };
            draw_triangle(
                vec2(m * s - 12.0 * s - nudge + shake, cy),
                vec2(m * s - 6.0 * s + shake, cy - 4.0 * s),
                vec2(m * s - 6.0 * s + shake, cy + 4.0 * s), dim(media_sel > 0));
            draw_triangle(
                vec2(x + 6.0 * s + nudge, cy),
                vec2(x, cy - 4.0 * s),
                vec2(x, cy + 4.0 * s), dim(media_sel + 1 < media_ids.len()));
        }
    }
    // The active-medium underline slides rather than teleports, so a storage
    // switch reads as motion instead of a repaint.
    let (ul_x, ul_w) = SAVE_ANIM.with(|cell| {
        let mut a = cell.borrow_mut();
        if a.ul_w <= 0.0 {
            a.ul_x = active_ul.0;
            a.ul_w = active_ul.1;
        }
        let k = (get_frame_time() / 0.09).min(1.0);
        a.ul_x += (active_ul.0 - a.ul_x) * k;
        a.ul_w += (active_ul.1 - a.ul_w) * k;
        (a.ul_x, a.ul_w)
    });
    if !media_ids.is_empty() && ul_w > 0.0 {
        let (bar_h, bar_col) = if grid_focus {
            (2.0 * s, XBOX_GREEN)
        } else {
            (3.0 * s, string_to_color(&config.cursor_color))
        };
        draw_rectangle(ul_x, strip_y + 5.0 * s, ul_w, bar_h, bar_col);
    }

    // Sub-caption: free space on the left, page range on the right.
    if let Some(free) = media_free.get(media_sel) {
        let free_txt = if *free >= 1024 {
            format!("{:.1} GB free", *free as f32 / 1024.0)
        } else {
            format!("{} MB free", free)
        };
        txt(&free_txt, m * s, 82.0 * s - header_drop, fs(0.72), Color::new(1.0, 1.0, 1.0, 0.55));
    }
    if memories.len() > SAVE_COLS * SAVE_ROWS {
        let first = scroll_offset * SAVE_COLS + 1;
        let last = (first + SAVE_COLS * SAVE_ROWS - 1).min(memories.len());
        let label = format!("{}–{} of {}", first, last, memories.len());
        let d = measure_text(&label, Some(font), fs(0.72), 1.0);
        txt(&label, content_r * s - d.width, 82.0 * s - header_drop, fs(0.72),
            Color::new(1.0, 1.0, 1.0, 0.45));
    }

    // --- The wall. Tiles fly in along the same rays the dashboard's Save Data
    // tile threw its icons out on, so the burst you just watched reassembles
    // into this grid. ---
    let burst_origin = {
        let r = tile_rect(&TABS[0].tiles[0], TABS[0].tiles, ORIGIN_X * s, 112.0 * s, s);
        vec2(r.x + r.w / 2.0, r.y + r.h / 2.0)
    };
    let slot_rect = |i: usize| -> (f32, f32) {
        let col = (i % SAVE_COLS) as f32;
        let row = (i / SAVE_COLS) as f32;
        (
            (m + col * (tile_w + SAVE_GAP)) * s,
            (SAVE_GRID_TOP + row * (SAVE_TILE_H + SAVE_GAP)) * s,
        )
    };
    let tw = tile_w * s;
    let th = SAVE_TILE_H * s;

    // Entry wave per slot, plus the gentler reflow wave for scrolls and list
    // changes.
    let entry = |i: usize| -> (f32, Vec2, f32) {
        let q = ease_out(((intro - i as f32 * 0.028) / 0.62).clamp(0.0, 1.0));
        let (x, y) = slot_rect(i);
        let c = vec2(x + tw * 0.5, y + th * 0.5);
        let mut dir = (c - burst_origin).normalize_or_zero();
        if dir == Vec2::ZERO {
            dir = vec2(0.0, -1.0);
        }
        let dist = (1.0 - q) * (240.0 + (i % 3) as f32 * 70.0) * s;
        (q, vec2(dir.x * dist, dir.y * dist * 0.8), q)
    };
    let reflow = |i: usize| -> f32 {
        ease_out(((pop - (i % SAVE_COLS) as f32 * 0.012 - (i / SAVE_COLS) as f32 * 0.030) / 0.55)
            .clamp(0.0, 1.0))
    };

    let draw_slot = |i: usize, focused: bool, shrinking: bool| {
        let (q, off, tile_alpha) = entry(i);
        let p = reflow(i);
        let (bx, by) = slot_rect(i);
        let occupied = i < on_page;
        let mem = memories.get(SAVE_COLS * scroll_offset + i);

        let scale = if focused {
            1.0 + (SEL_SCALE - 1.0) * ease_out((sel_anim / SEL_GROW_TIME).min(1.0))
        } else if shrinking {
            1.0 + (SEL_SCALE - 1.0) * (1.0 - ease_out_sine((sel_anim / SEL_SHRINK_TIME).min(1.0)))
        } else {
            1.0
        };
        let assemble = (0.90 + 0.10 * q) * (0.92 + 0.08 * p);
        let sc = scale * assemble;
        let ghost_pull = if occupied { 1.0 } else { 0.5 };
        let cx = bx + tw * 0.5 + off.x * ghost_pull;
        let cy = by + th * 0.5 + off.y * ghost_pull + (1.0 - p) * 8.0 * s;
        let rw = tw * sc;
        let rh = th * sc;
        let rx = cx - rw * 0.5;
        let ry = cy - rh * 0.5;
        let alpha = tile_alpha * p * if occupied { 1.0 } else { 0.5 };
        if alpha <= 0.01 {
            return;
        }

        if !occupied {
            // Empty slot: just a whisper of a plate, so the wall reads as a
            // card with capacity rather than a void.
            draw_rectangle(rx, ry, rw, rh, Color::new(1.0, 1.0, 1.0, 0.045 * alpha));
            return;
        }

        let lift = ((scale - 1.0) / (SEL_SCALE - 1.0)).clamp(0.0, 1.0);
        if lift > 0.01 {
            draw_tile_shadow(rx, ry, rw, rh, s, lift);
        }
        let col = i % SAVE_COLS;
        let row = i / SAVE_COLS;
        let fill = if focused {
            TILE_FOCUS
        } else if (col + row) % 2 == 0 {
            TILE_SLATE
        } else {
            TILE_SLATE_ALT
        };
        draw_rectangle(rx, ry, rw, rh, Color::new(fill.r, fill.g, fill.b, alpha));

        // Icon on the pixel grid: a fixed integer size regardless of the
        // tile's focus scale, so nearest-neighbour never drops rows on the
        // one tile you're looking at.
        if let Some(mem) = mem {
            let icon = icon_cache.get(&mem.id).unwrap_or(placeholder);
            icon.set_filter(FilterMode::Nearest);
            let ip = (32.0 * s).round();
            let iq = 1.0 + 1.4 * (1.0 - q);
            let isz = (ip * iq).round();
            draw_texture_ex(
                icon,
                (rx + (rw - isz) * 0.5).round(),
                (ry + 6.0 * s * sc).round(),
                Color::new(1.0, 1.0, 1.0, q.powf(0.6) * p),
                DrawTextureParams { dest_size: Some(vec2(isz, isz)), ..Default::default() },
            );
        }

        FADE_TEX.with(|tex| {
            draw_texture_ex(tex, rx, ry + rh - rh * 0.30, Color::new(0.0, 0.0, 0.0, 0.72 * alpha),
                DrawTextureParams { dest_size: Some(vec2(rw, rh * 0.30)), ..Default::default() });
        });

        if let Some(mem) = mem {
            // An absent OR empty Name attribute both fall back to the id, so a
            // tile is never nameless.
            let name = mem.name.clone().filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| mem.id.clone());
            let (label, size) = fit(&name, 0.72, rw - 12.0 * s);
            let c = if focused { 1.0 } else { 0.82 };
            txt(&label, rx + 6.0 * s, ry + rh - 6.0 * s, size, Color::new(1.0, 1.0, 1.0, c * alpha));
        }

        if focused && grid_focus {
            draw_focus_glow_ex(rx, ry, rw, rh, s, string_to_color(&config.cursor_color), 0.75, 1.5);
        }
    };

    // Ghosts first, then resting tiles, then the one shrinking back, then the
    // focused tile last so it overlaps its neighbours.
    for i in on_page..(SAVE_COLS * SAVE_ROWS) {
        draw_slot(i, false, false);
    }
    for i in 0..on_page {
        if i != selected_memory && Some(i) != prev_sel {
            draw_slot(i, false, false);
        }
    }
    if let Some(p) = prev_sel {
        if p < on_page && p != selected_memory {
            draw_slot(p, false, true);
        }
    }
    if selected_memory < on_page {
        draw_slot(selected_memory, true, false);
    }

    // Scroll rail and overflow chevrons.
    if rows_total > SAVE_ROWS {
        let rail_x = (content_r + 6.0) * s;
        let rail_y = SAVE_GRID_TOP * s;
        let rail_h = (SAVE_ROWS as f32 * SAVE_TILE_H + (SAVE_ROWS as f32 - 1.0) * SAVE_GAP) * s;
        let a = ease_out(intro);
        draw_rectangle(rail_x, rail_y, 3.0 * s, rail_h, Color::new(1.0, 1.0, 1.0, 0.10 * a));
        let thumb_h = (rail_h * SAVE_ROWS as f32 / rows_total as f32).max(18.0 * s);
        let span = (rows_total - SAVE_ROWS) as f32;
        let ty = rail_y + (rail_h - thumb_h) * (scroll_offset as f32 / span.max(1.0));
        let tc = if grid_focus { string_to_color(&config.cursor_color) } else { Color::new(1.0, 1.0, 1.0, 1.0) };
        draw_rectangle(rail_x, ty, 3.0 * s, thumb_h, Color::new(tc.r, tc.g, tc.b, 0.75 * a));

        let nudge = (get_time() as f32 * 2.4).sin() * 1.0 * s;
        let mid = (m + content_w * 0.5) * s;
        let chev = Color::new(1.0, 1.0, 1.0, 0.50 * a);
        if scroll_offset > 0 {
            let cy = 84.0 * s;
            draw_triangle(vec2(mid, cy - 4.0 * s - nudge), vec2(mid - 4.0 * s, cy), vec2(mid + 4.0 * s, cy), chev);
        }
        if SAVE_COLS * (SAVE_ROWS + scroll_offset) < memories.len() {
            let cy = 266.0 * s;
            draw_triangle(vec2(mid, cy + 4.0 * s + nudge), vec2(mid - 4.0 * s, cy), vec2(mid + 4.0 * s, cy), chev);
        }
    }

    // --- Detail bar: the Play hero's translucent bar, describing whatever
    // holds the cursor ---
    {
        let bar_rise = (1.0 - ease_out((intro / 0.40).min(1.0))) * 70.0 * s;
        let bx = m * s;
        let by = 270.0 * s + bar_rise;
        let bw = content_w * s;
        let bh = 36.0 * s;
        let a = ease_out(intro);
        draw_rectangle(bx, by, bw, bh, Color::new(0.0, 0.0, 0.0, 0.55 * a));
        FADE_TEX.with(|tex| {
            draw_texture_ex(tex, bx, by, Color::new(1.0, 1.0, 1.0, 0.06 * a), DrawTextureParams {
                dest_size: Some(vec2(bw, 7.2 * s)), flip_y: true, ..Default::default()
            });
        });
        draw_rectangle(bx, by, 3.0 * s, bh, Color::new(XBOX_GREEN.r, XBOX_GREEN.g, XBOX_GREEN.b, a));

        let name_x = bx + 48.0 * s;
        let kick = (1.0 - (sel_anim / 0.12).min(1.0)) * 4.0 * s;
        let text_a = (0.35 + 0.65 * (sel_anim / 0.12).min(1.0)) * a;
        let mem = if grid_focus {
            memories.get(SAVE_COLS * scroll_offset + selected_memory)
        } else {
            None
        };

        let (title, meta, right_top) = if let Some(mem) = mem {
            let icon = icon_cache.get(&mem.id).unwrap_or(placeholder);
            icon.set_filter(FilterMode::Nearest);
            let ip = (26.0 * s).round();
            draw_texture_ex(icon, bx + 12.0 * s, by + 5.0 * s, Color::new(1.0, 1.0, 1.0, a),
                DrawTextureParams { dest_size: Some(vec2(ip, ip)), ..Default::default() });
            // An absent OR empty Name attribute both fall back to the id, so a
            // tile is never nameless.
            let name = mem.name.clone().filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| mem.id.clone());
            (
                name,
                format!(
                    "{:.1} MB  ·  {:.1} h  ·  {}",
                    get_game_size(mem, size_cache),
                    get_game_playtime(mem, playtime_cache),
                    mem.drive_name.to_lowercase()
                ),
                media_free.get(media_sel).map(|f| if *f >= 1024 {
                    format!("{:.1} GB free", *f as f32 / 1024.0)
                } else {
                    format!("{} MB free", f)
                }).unwrap_or_default(),
            )
        } else if media_ids.is_empty() {
            ("No storage detected".to_string(), "Insert a card or restart the console".to_string(), String::new())
        } else if !grid_focus {
            // The strip holds the cursor: the bar describes the device.
            let id = media_ids.get(media_sel).cloned().unwrap_or_default();
            if id.to_lowercase() == "internal" {
                draw_rectangle(bx + 12.0 * s, by + 11.0 * s, 20.0 * s, 14.0 * s, Color::new(1.0, 1.0, 1.0, 0.85 * a));
                draw_rectangle(bx + 15.0 * s, by + 14.0 * s, 14.0 * s, 1.0 * s, Color::new(0.15, 0.15, 0.15, a));
                draw_rectangle(bx + 15.0 * s, by + 17.0 * s, 14.0 * s, 1.0 * s, Color::new(0.15, 0.15, 0.15, a));
            } else {
                let ip = (26.0 * s).round();
                draw_texture_ex(&state.badge_sd, bx + 12.0 * s, by + 5.0 * s,
                    Color::new(1.0, 1.0, 1.0, 0.85 * a),
                    DrawTextureParams { dest_size: Some(vec2(ip, ip)), ..Default::default() });
            }
            (
                id.to_lowercase(),
                format!("{} saves", memories.len()),
                media_free.get(media_sel).map(|f| if *f >= 1024 {
                    format!("{:.1} GB free", *f as f32 / 1024.0)
                } else {
                    format!("{} MB free", f)
                }).unwrap_or_default(),
            )
        } else {
            let id = media_ids.get(media_sel).cloned().unwrap_or_default();
            ("No save data".to_string(), format!("on {}", id.to_lowercase()), "0 saves".to_string())
        };

        let (t, tsize) = fit(&title, 1.10, bw - 200.0 * s);
        txt(&t, name_x + kick, by + 18.0 * s, tsize, Color::new(1.0, 1.0, 1.0, 0.95 * text_a));
        let (mt, msize) = fit(&meta, 0.72, bw - 200.0 * s);
        txt(&mt, name_x + kick, by + 31.0 * s, msize, Color::new(1.0, 1.0, 1.0, 0.62 * text_a));
        if !right_top.is_empty() {
            let d = measure_text(&right_top, Some(font), fs(0.85), 1.0);
            txt(&right_top, bx + bw - 12.0 * s - d.width, by + 18.0 * s, fs(0.85),
                Color::new(1.0, 1.0, 1.0, 0.88 * a));
        }
        let counter = if grid_focus && !memories.is_empty() {
            format!("save {} of {}", SAVE_COLS * scroll_offset + selected_memory + 1, memories.len())
        } else if !grid_focus && !media_ids.is_empty() {
            format!("storage {} of {}", media_sel + 1, media_ids.len())
        } else {
            String::new()
        };
        if !counter.is_empty() {
            let d = measure_text(&counter, Some(font), fs(0.72), 1.0);
            txt(&counter, bx + bw - 12.0 * s - d.width, by + 31.0 * s, fs(0.72),
                Color::new(1.0, 1.0, 1.0, 0.45 * a));
        }
    }

    // --- Legend, suppressed while a dialog owns the screen ---
    if *dialog_state == DialogState::None {
        let cy = 324.0 * s;
        let icon_h = 18.0 * s;
        let size = fs(0.80);
        let leg = |text: &str, x: f32| -> f32 {
            let d = measure_text(text, Some(font), size, 1.0);
            txt(text, x - d.width, cy + d.offset_y * 0.5, size, Color::new(1.0, 1.0, 1.0, 0.75 * overlay_a));
            x - d.width
        };
        let back_x = leg("Back", w - 24.0 * s);
        LEGEND_ICONS_BACK.with(|icons| {
            draw_texture_ex(&icons[legend_now as usize], back_x - 4.0 * s - icon_h, cy - icon_h / 2.0,
                Color::new(1.0, 1.0, 1.0, overlay_a),
                DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() });
        });
        let mut gx = back_x - 4.0 * s - icon_h - 16.0 * s;
        if grid_focus && !memories.is_empty() {
            let x = leg("Manage", gx);
            LEGEND_ICONS.with(|icons| {
                draw_texture_ex(&icons[legend_now as usize], x - 4.0 * s - icon_h, cy - icon_h / 2.0,
                    Color::new(1.0, 1.0, 1.0, overlay_a),
                    DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() });
            });
            gx = x - 4.0 * s - icon_h - 16.0 * s;
        }
        if !grid_focus && media_ids.len() > 1 {
            let x = leg("Storage", gx);
            let bx2 = x - 4.0 * s - icon_h;
            let c = Color::new(1.0, 1.0, 1.0, 0.75 * overlay_a);
            let nudge = (get_time() as f32 * 2.4).sin() * 1.0 * s;
            draw_triangle(vec2(bx2 - nudge, cy), vec2(bx2 + 6.0 * s, cy - 5.0 * s), vec2(bx2 + 6.0 * s, cy + 5.0 * s), c);
            draw_triangle(vec2(bx2 + icon_h + nudge, cy), vec2(bx2 + icon_h - 6.0 * s, cy - 5.0 * s), vec2(bx2 + icon_h - 6.0 * s, cy + 5.0 * s), c);
        }

        // Shoulder hint, same geometry as the settings screen so the two line up.
        if media_ids.len() > 1 {
            let (lb, rb) = match legend_now {
                LegendIcon::Keyboard => ("[", "]"),
                LegendIcon::PlayStation => ("L1", "R1"),
                LegendIcon::Switch | LegendIcon::Switch2 | LegendIcon::N64 => ("L", "R"),
                _ => ("LB", "RB"),
            };
            let psize = fs(0.62);
            let cw = 15.0 * s;
            let chh = 11.0 * s;
            let pcy = 318.0 * s;
            for (i, name) in [lb, rb].iter().enumerate() {
                let px2 = (m + i as f32 * 18.0) * s;
                draw_rectangle(px2, pcy, cw, chh, Color::new(0.24, 0.25, 0.26, 0.85 * overlay_a));
                let d = measure_text(name, Some(font), psize, 1.0);
                txt(name, px2 + (cw - d.width) / 2.0, pcy + chh / 2.0 + d.offset_y * 0.5, psize,
                    Color::new(1.0, 1.0, 1.0, 0.85 * overlay_a));
            }
            txt("Storage", (m + 38.0) * s, pcy + chh / 2.0 + 3.0 * s, fs(0.68),
                Color::new(1.0, 1.0, 1.0, 0.55 * overlay_a));
        }
    }

    render_ui_overlay_alpha(logo_cache, font_cache, config, battery_info, current_time_str,
        gcc_adapter_poll_rate, s, overlay_a, true, true);

    // Veil under any dialog, eased so even the delete path (which jumps
    // straight back to None with no transition) fades rather than snaps.
    if veil > 0.001 {
        draw_rectangle(0.0, 0.0, w, h, Color::new(0.0, 0.0, 0.0, veil));
    }
}

/// Metro sheet for the save dialogs: manage, copy-to, confirm-delete, the
/// already-exists notice, errors, and the copy progress meter. The dialog
/// constructors in ui/dialog.rs are untouched — same ids, same option values,
/// same default selections — so update() behaves exactly as before.
pub fn draw_save_dialog(
    dialog: &Dialog,
    memories: &[Memory],
    selected_memory: usize,
    icon_cache: &HashMap<String, Texture2D>,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    copy_op_state: &Arc<Mutex<CopyOperationState>>,
    placeholder: &Texture2D,
    scroll_offset: usize,
    storage_state: &Arc<Mutex<StorageMediaState>>,
    animation_state: &AnimationState,
    playtime_cache: &mut PlaytimeCache,
    size_cache: &mut SizeCache,
    s: f32,
) {
    let font = get_current_font(font_cache, config);
    let w = screen_width();
    let h = screen_height();
    let txt = |text: &str, x: f32, y: f32, size: u16, color: Color| {
        let so = 1.0 * (size as f32 / FONT_SIZE as f32);
        draw_text_ex(text, x + so, y + so, TextParams {
            font: Some(font), font_size: size,
            color: Color::new(0.0, 0.0, 0.0, 0.85 * color.a), ..Default::default()
        });
        draw_text_ex(text, x, y, TextParams {
            font: Some(font), font_size: size, color, ..Default::default()
        });
    };
    let fs = |k: f32| ((FONT_SIZE as f32 * s * k) as u16).max(9);
    let fit = |text: &str, k: f32, max_w: f32| -> (String, u16) {
        let mut size = fs(k);
        let d = measure_text(text, Some(font), size, 1.0);
        if d.width > max_w && d.width > 0.0 {
            size = (((size as f32) * max_w / d.width).floor() as u16).max(9);
        }
        let mut out = text.to_string();
        if measure_text(&out, Some(font), size, 1.0).width > max_w {
            while out.chars().count() > 1
                && measure_text(&format!("{}…", out), Some(font), size, 1.0).width > max_w
            {
                out.pop();
            }
            out.push('…');
        }
        (out, size)
    };

    // Snapshot once: `running` flips from the copy worker thread and a
    // mid-frame change would draw a sheet with neither rows nor meter.
    let (copy_progress, copy_running) = copy_op_state
        .lock()
        .map(|st| (st.progress, st.running))
        .unwrap_or((0, false));
    let legend = SAVE_ANIM.with(|c| c.borrow().legend);
    // Local index: ui/mod.rs's get_memory_index is hard-wired to the classic
    // 13-wide grid and would name a different save here.
    let memory_index = selected_memory + SAVE_COLS * scroll_offset;
    let subject = memories.get(memory_index);

    let destructive = dialog.id == "confirm_delete" || dialog.id == "error";
    let eyebrow = match dialog.id.as_str() {
        "main" => "manage",
        "copy_storage_select" => "copy to",
        "confirm_delete" => "delete",
        "save_exists" => "notice",
        _ => "error",
    };

    // --- Sheet geometry ---
    let pw = 320.0;
    let pwd = pw * s;
    let px = (w - pwd) / 2.0;
    let desc_lines: Vec<String> = match &dialog.desc {
        Some(d) => {
            let size = fs(0.90);
            let max_w = (pw - 32.0) * s;
            let mut out = Vec::new();
            let mut line = String::new();
            for word in d.split_whitespace() {
                let probe = if line.is_empty() { word.to_string() } else { format!("{} {}", line, word) };
                if measure_text(&probe, Some(font), size, 1.0).width <= max_w {
                    line = probe;
                } else {
                    out.push(std::mem::take(&mut line));
                    line = word.to_string();
                }
            }
            if !line.is_empty() {
                out.push(line);
            }
            out.truncate(4);
            out
        }
        None => Vec::new(),
    };
    let desc_h = if desc_lines.is_empty() { 0.0 } else { desc_lines.len() as f32 * 14.0 + 8.0 };
    let n = dialog.options.len().max(1) as f32;
    // Pitch shrinks rather than letting the sheet run off screen when a
    // console has many mounted media.
    let pitch = if copy_running { 27.0 } else { ((340.0 - 66.0 - desc_h - 16.0) / n).clamp(18.0, 27.0) };
    let block_h = if copy_running { 60.0 } else { n * pitch - 3.0 };
    let ph = (66.0 + desc_h + block_h + 16.0).min(340.0);
    let phd = ph * s;
    let py = (h - phd) / 2.0;

    draw_tile_shadow(px, py, pwd, phd, s, 1.0);
    draw_rectangle(px, py, pwd, phd, TILE_SLATE);
    FADE_TEX.with(|tex| {
        draw_texture_ex(tex, px, py, Color::new(1.0, 1.0, 1.0, 0.20), DrawTextureParams {
            dest_size: Some(vec2(pwd, phd * 0.16)), flip_y: true, ..Default::default()
        });
        draw_texture_ex(tex, px, py + phd * 0.72, Color::new(0.0, 0.0, 0.0, 0.45), DrawTextureParams {
            dest_size: Some(vec2(pwd, phd * 0.28)), ..Default::default()
        });
    });
    draw_rectangle(px, py, 3.0 * s, phd, if destructive { TILE_RED } else { XBOX_GREEN });

    // --- Subject header: you can never act on a save the sheet hasn't named ---
    if let Some(mem) = subject {
        let icon = icon_cache.get(&mem.id).unwrap_or(placeholder);
        icon.set_filter(FilterMode::Nearest);
        let ip = (32.0 * s).round();
        draw_texture_ex(icon, px + 16.0 * s, py + 14.0 * s, WHITE,
            DrawTextureParams { dest_size: Some(vec2(ip, ip)), ..Default::default() });
        let name = mem.name.clone().filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| mem.id.clone());
        let (t, tsize) = fit(&name, 1.10, pwd - 74.0 * s);
        txt(&t, px + 62.0 * s, py + 32.0 * s, tsize, Color::new(1.0, 1.0, 1.0, 0.95));
        let meta = format!(
            "{:.1} MB · {:.1} h",
            get_game_size(mem, size_cache),
            get_game_playtime(mem, playtime_cache)
        );
        txt(&meta, px + 62.0 * s, py + 47.0 * s, fs(0.72), Color::new(1.0, 1.0, 1.0, 0.60));
    }
    {
        let d = measure_text(eyebrow, Some(font), fs(0.68), 1.0);
        txt(eyebrow, px + pwd - 16.0 * s - d.width, py + 32.0 * s, fs(0.68),
            Color::new(1.0, 1.0, 1.0, 0.45));
    }
    draw_rectangle(px + 16.0 * s, py + 58.0 * s, pwd - 32.0 * s, 1.0 * s, Color::new(1.0, 1.0, 1.0, 0.12));

    for (i, line) in desc_lines.iter().enumerate() {
        txt(line, px + 16.0 * s, py + (76.0 + i as f32 * 14.0) * s, fs(0.90),
            Color::new(1.0, 1.0, 1.0, 0.85));
    }

    let oy = py + (66.0 + desc_h) * s;

    if copy_running {
        // Same 10-cell meter the settings screen uses for percentages.
        let done = copy_progress >= 100;
        txt(if done { "Copy complete" } else { "Copying…" }, px + 16.0 * s, oy + 16.0 * s,
            fs(0.90), Color::new(1.0, 1.0, 1.0, 0.90));
        let pct = format!("{}%", copy_progress.min(100));
        let d = measure_text(&pct, Some(font), fs(0.72), 1.0);
        txt(&pct, px + pwd - 16.0 * s - d.width, oy + 16.0 * s, fs(0.72), Color::new(1.0, 1.0, 1.0, 0.60));
        let total = pwd - 32.0 * s;
        let gap = 1.6 * s;
        let cw = (total - 9.0 * gap) / 10.0;
        let filled = ((copy_progress as f32 / 10.0).round() as i32).clamp(0, 10) as usize;
        let fill_col = if done { XBOX_GREEN } else { string_to_color(&config.cursor_color) };
        for i in 0..10 {
            let c = if i < filled { fill_col } else { Color::new(1.0, 1.0, 1.0, 0.16) };
            draw_rectangle(px + 16.0 * s + i as f32 * (cw + gap), oy + 26.0 * s, cw, 9.0 * s, c);
        }
    } else {
        let free_by_id: Vec<(String, u32)> = storage_state
            .lock()
            .map(|st| st.media.iter().map(|d| (d.id.clone(), d.free)).collect())
            .unwrap_or_default();
        let subject_mb = subject.map(|m| get_game_size(m, size_cache)).unwrap_or(0.0);

        for (i, opt) in dialog.options.iter().enumerate() {
            let focused = i == dialog.selection;
            let delete_row = opt.value == "DELETE";
            let shake = if opt.disabled {
                animation_state.calculate_shake_offset(ShakeTarget::Dialog) * s
            } else {
                0.0
            };
            let rx = px + 16.0 * s + shake;
            let ry = oy + i as f32 * pitch * s;
            let rw = pwd - 32.0 * s;
            let rh = (pitch - 3.0) * s;

            // A disabled row keeps the cursor visible but never looks
            // actionable — the main sheet opens with COPY disabled and
            // focused on a single-storage console.
            let fill = if opt.disabled {
                Color::new(TILE_SLATE_ALT.r * 0.45, TILE_SLATE_ALT.g * 0.45, TILE_SLATE_ALT.b * 0.45, 1.0)
            } else if delete_row {
                if focused { TILE_RED } else { TILE_RED_DIM }
            } else if focused {
                XBOX_GREEN
            } else {
                TILE_SLATE_ALT
            };
            if focused {
                draw_tile_shadow(rx, ry, rw, rh, s, 1.0);
            }
            draw_rectangle(rx, ry, rw, rh, fill);

            let label = dlg_label(&opt.value);
            let label = if dialog.id == "copy_storage_select" && opt.value != "CANCEL" {
                opt.value.to_lowercase()
            } else {
                label
            };
            let lcol = if opt.disabled {
                Color::new(1.0, 1.0, 1.0, 0.40)
            } else if focused {
                WHITE
            } else {
                Color::new(1.0, 1.0, 1.0, 0.85)
            };
            let (lt, lsize) = fit(&label, 0.85, rw * 0.55);
            let d = measure_text(&lt, Some(font), lsize, 1.0);
            txt(&lt, rx + 10.0 * s, ry + rh / 2.0 + d.offset_y * 0.5, lsize, lcol);

            // Right-hand hint: say why, instead of only shaking.
            let mut hint = String::new();
            let mut hint_col = Color::new(1.0, 1.0, 1.0, 0.55);
            if dialog.id == "main" {
                hint = match opt.value.as_str() {
                    "COPY" => if opt.disabled { "no other storage".into() } else { "to another storage".into() },
                    "DELETE" => "permanent".to_string(),
                    _ => String::new(),
                };
            } else if dialog.id == "copy_storage_select" && opt.value != "CANCEL" {
                if let Some((_, free)) = free_by_id.iter().find(|(id, _)| *id == opt.value) {
                    if subject_mb > *free as f32 {
                        hint = "not enough space".to_string();
                        hint_col = Color::new(1.0, 0.42, 0.42, 1.0);
                        draw_rectangle(rx, ry, 2.0 * s, rh, Color::new(1.0, 0.42, 0.42, 1.0));
                    } else {
                        hint = format!("{} MB free", free);
                    }
                }
            }
            let glyph_w = if focused && !opt.disabled { 26.0 * s } else { 10.0 * s };
            if !hint.is_empty() {
                let (ht, hsize) = fit(&hint, 0.68, rw * 0.42);
                let hd = measure_text(&ht, Some(font), hsize, 1.0);
                txt(&ht, rx + rw - glyph_w - hd.width, ry + rh / 2.0 + hd.offset_y * 0.5, hsize, hint_col);
            }
            if focused && !opt.disabled {
                let g = 12.0 * s;
                LEGEND_ICONS.with(|icons| {
                    draw_texture_ex(&icons[legend], rx + rw - 10.0 * s - g, ry + rh / 2.0 - g / 2.0,
                        WHITE, DrawTextureParams { dest_size: Some(vec2(g, g)), ..Default::default() });
                });
            }
            if focused {
                let glow = if opt.disabled {
                    Color::new(0.6, 0.6, 0.6, 1.0)
                } else if delete_row {
                    Color::new(1.0, 0.35, 0.35, 1.0)
                } else {
                    string_to_color(&config.cursor_color)
                };
                draw_focus_glow_ex(rx, ry, rw, rh, s, glow, 0.45, 1.0);
            }
        }
    }

    // --- Dialog legend ---
    {
        let cy = 324.0 * s;
        let icon_h = 18.0 * s;
        let size = fs(0.80);
        let leg = |text: &str, x: f32| -> f32 {
            let d = measure_text(text, Some(font), size, 1.0);
            txt(text, x - d.width, cy + d.offset_y * 0.5, size, Color::new(1.0, 1.0, 1.0, 0.75));
            x - d.width
        };
        let back_x = leg("Cancel", w - 24.0 * s);
        LEGEND_ICONS_BACK.with(|icons| {
            draw_texture_ex(&icons[legend], back_x - 4.0 * s - icon_h, cy - icon_h / 2.0, WHITE,
                DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() });
        });
        if !copy_running {
            let focused_delete = dialog
                .options
                .get(dialog.selection)
                .map(|o| o.value == "DELETE")
                .unwrap_or(false);
            let x = leg(if focused_delete { "Delete" } else { "Select" },
                back_x - 4.0 * s - icon_h - 16.0 * s);
            LEGEND_ICONS.with(|icons| {
                draw_texture_ex(&icons[legend], x - 4.0 * s - icon_h, cy - icon_h / 2.0, WHITE,
                    DrawTextureParams { dest_size: Some(vec2(icon_h, icon_h)), ..Default::default() });
            });
        }
    }
}

/// Metro skin for the modal dialogs (reset confirmation and its follow-up).
/// Same slate-and-green vocabulary as the tiles, so the settings screen
/// doesn't drop out of its own language for the one screen that matters most.
pub fn draw_dialog(
    message: &str,
    options: Option<(&str, &str)>,
    selection: usize,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    s: f32,
) {
    let font = get_current_font(font_cache, config);
    let w = screen_width();
    let h = screen_height();
    let txt = |text: &str, x: f32, y: f32, size: u16, color: Color| {
        let so = 1.0 * (size as f32 / FONT_SIZE as f32);
        draw_text_ex(text, x + so, y + so, TextParams {
            font: Some(font), font_size: size,
            color: Color::new(0.0, 0.0, 0.0, 0.85 * color.a), ..Default::default()
        });
        draw_text_ex(text, x, y, TextParams {
            font: Some(font), font_size: size, color, ..Default::default()
        });
    };
    let fs = |k: f32| ((FONT_SIZE as f32 * s * k) as u16).max(9);

    draw_rectangle(0.0, 0.0, w, h, Color::new(0.0, 0.0, 0.0, 0.6));

    let bw = 300.0 * s;
    let bh = 132.0 * s;
    let bx = (w - bw) / 2.0;
    let by = (h - bh) / 2.0;
    draw_rectangle(bx, by, bw, bh, TILE_SLATE);
    FADE_TEX.with(|tex| {
        draw_texture_ex(tex, bx, by, Color::new(1.0, 1.0, 1.0, 0.20), DrawTextureParams {
            dest_size: Some(vec2(bw, bh * 0.16)), flip_y: true, ..Default::default()
        });
        draw_texture_ex(tex, bx, by + bh * 0.72, Color::new(0.0, 0.0, 0.0, 0.45), DrawTextureParams {
            dest_size: Some(vec2(bw, bh * 0.28)), ..Default::default()
        });
    });
    // Leading green bar, the dash's "this is actionable" mark.
    draw_rectangle(bx, by, 3.0 * s, bh, XBOX_GREEN);

    // Eyebrow with the settings mark.
    let icon = 14.0 * s;
    TILE_SETTINGS.with(|tex| {
        draw_texture_ex(tex, bx + 14.0 * s, by + 12.0 * s, Color::new(1.0, 1.0, 1.0, 0.75),
            DrawTextureParams { dest_size: Some(vec2(icon, icon)), ..Default::default() });
    });
    txt("settings", bx + 14.0 * s + icon + 6.0 * s, by + 23.0 * s, fs(0.8),
        Color::new(1.0, 1.0, 1.0, 0.45));

    // Message, centred.
    let msg_size = fs(0.95);
    let mut y = by + 56.0 * s;
    for line in message.lines() {
        let d = measure_text(line, Some(font), msg_size, 1.0);
        txt(line, bx + (bw - d.width) / 2.0, y, msg_size, Color::new(1.0, 1.0, 1.0, 0.92));
        y += d.height + 6.0 * s;
    }

    // Choices as Metro chips; the focused one takes the green fill and glow.
    if let Some((opt1, opt2)) = options {
        let cw = 84.0 * s;
        let ch = 24.0 * s;
        let gap = 12.0 * s;
        let total = cw * 2.0 + gap;
        let cx = bx + (bw - total) / 2.0;
        let cy = by + bh - ch - 14.0 * s;
        for (i, label) in [opt1, opt2].iter().enumerate() {
            let x = cx + i as f32 * (cw + gap);
            let focused = i == selection;
            draw_rectangle(x, cy, cw, ch, if focused { XBOX_GREEN } else { TILE_SLATE_ALT });
            let size = fs(0.85);
            let d = measure_text(label, Some(font), size, 1.0);
            txt(label, x + (cw - d.width) / 2.0, cy + ch / 2.0 + d.offset_y * 0.5, size,
                if focused { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.82) });
            if focused {
                draw_focus_glow_ex(x, cy, cw, ch, s, string_to_color(&config.cursor_color), 0.5, 1.2);
            }
        }
    }
}

const GUIDE_ITEMS: &[&str] = &["Close Guide", "Kazeta Home", "Power Off"];

/// Metro-styled guide modal, drawn over whatever screen is active. Call after
/// the screen has rendered, feeding it the pre-suppression input snapshot.
/// Returns the action the caller should perform.
pub fn guide_overlay(
    state: &mut GuideState,
    up: bool,
    down: bool,
    select: bool,
    back: bool,
    sound_effects: &SoundEffects,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    s: f32,
) -> GuideAction {
    if up {
        state.selection = if state.selection == 0 { GUIDE_ITEMS.len() - 1 } else { state.selection - 1 };
        sound_effects.play_cursor_move(config);
    }
    if down {
        state.selection = (state.selection + 1) % GUIDE_ITEMS.len();
        sound_effects.play_cursor_move(config);
    }

    let mut action = GuideAction::None;
    if back {
        state.open = false;
        sound_effects.play_back(config);
    } else if select {
        match state.selection {
            0 => {
                state.open = false;
                sound_effects.play_back(config);
            }
            1 => {
                state.open = false;
                action = GuideAction::KazetaHome;
                sound_effects.play_select(config);
            }
            2 => {
                action = GuideAction::PowerOff;
                sound_effects.play_select(config);
            }
            _ => {}
        }
    }

    // Dim the world, then a small Metro panel dead center.
    draw_rectangle(0.0, 0.0, screen_width(), screen_height(), Color::new(0.0, 0.0, 0.0, 0.6));

    let pw = 190.0 * s;
    let ph = 122.0 * s;
    let px = (screen_width() - pw) / 2.0;
    let py = (screen_height() - ph) / 2.0;

    const STRIPS: usize = 8;
    let strip_h = ph / STRIPS as f32;
    for strip in 0..STRIPS {
        let t = strip as f32 / (STRIPS - 1) as f32;
        let b = 1.06 - 0.12 * t;
        draw_rectangle(
            px, py + strip as f32 * strip_h, pw, strip_h + 1.0,
            Color::new(
                (TILE_SLATE.r * b).min(1.0),
                (TILE_SLATE.g * b).min(1.0),
                (TILE_SLATE.b * b).min(1.0),
                1.0,
            ),
        );
    }
    draw_rectangle_lines(px, py, pw, ph, 2.0 * s, WHITE);
    draw_rectangle_lines(
        px + 2.0 * s, py + 2.0 * s, pw - 4.0 * s, ph - 4.0 * s,
        1.0 * s, Color::new(0.0, 0.0, 0.0, 0.35),
    );

    let eyebrow = (FONT_SIZE as f32 * s * 0.8) as u16;
    text_with_color(
        font_cache, config, "guide",
        px + 12.0 * s, py + 18.0 * s, eyebrow,
        Color::new(1.0, 1.0, 1.0, 0.5),
    );

    let item_size = (FONT_SIZE as f32 * s) as u16;
    for (i, label) in GUIDE_ITEMS.iter().enumerate() {
        let y = py + 46.0 * s + i as f32 * 24.0 * s;
        let selected = i == state.selection;
        if selected {
            draw_rectangle(px + 8.0 * s, y - 12.0 * s, 3.0 * s, 15.0 * s, XBOX_GREEN);
        }
        let color = if selected { WHITE } else { Color::new(1.0, 1.0, 1.0, 0.45) };
        text_with_color(font_cache, config, label, px + 17.0 * s, y, item_size, color);
    }

    action
}
