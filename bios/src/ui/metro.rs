// Metro (2011+ Xbox 360 dashboard) main menu: lowercase tab strip up top,
// panes of flat tiles below — one big hero tile plus small tiles in a 2-row
// quad. Left/right walks tiles and crosses pane edges (or prev/next jumps a
// whole tab), up/down moves within a small-tile column, select activates.
// Enabled via `menu_style = "METRO"` in config.toml or a theme's theme.toml.

use crate::{
    Screen, InputState, render_background, render_ui_overlay, get_current_font, measure_text,
    text_with_config_color, FONT_SIZE,
    StorageMediaState, VideoPlayer, save,
    audio::SoundEffects,
    config::Config,
    types::{AnimationState, BackgroundState, BatteryInfo},
    ui::text_with_color,
    ui::blades::BladeAction,
    ui::main_menu::{activate_copy_logs, activate_play, activate_save_data},
};
use crate::audio::AUDIO;
use macroquad::prelude::*;
use rodio::{buffer::SamplesBuffer, Decoder, Sink, Source};
use std::io::Cursor;
use std::{
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
        title: "apps",
        tiles: &[
            MetroTile { label: "Themes", action: BladeAction::ThemeDownloader, hero: false, green: true, col: 0, row: 0 },
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

        Self {
            tab: DEFAULT_TAB, prev_tab: DEFAULT_TAB, anim: 1.0, dir: 1.0,
            tile: primary_tile(DEFAULT_TAB),
            cover_tex: None, icon_tex: None, cart_label: None, cart_optical: false,
            badge_sd, badge_disc, cover_key: String::new(),
            cart_vis: 0.0, outgoing: None,
            sel_anim: 1.0, prev_sel: None, press_flash: 0.0,
            bokeh, bokeh_tex: make_bokeh_texture(), fade_tex: make_fade_texture(),
            bgm_path: None, bgm_sink: None, bgm_vol: 0.0,
            mounts_fp: String::new(), mounts_polled: -10.0,
        }
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

/// Selection frame whose edge bars stop `r` short of the corners — reads as
/// dashx360's small corner radius at dash distance.
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
        state.stop_bgm();
        // The hover theme may have been ducking the system bgm when the cart
        // vanished — give the system its volume back.
        if let Some(system_bgm) = current_bgm.as_ref() {
            system_bgm.set_volume(1.0);
        }
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
                    }
                }
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
                activate_save_data(current_screen, input_state, storage_state, sound_effects, config);
            }
            BladeAction::Play => {
                if *play_option_enabled {
                    activate_play(
                        current_screen, sound_effects, config, log_messages, fade_start_time,
                        current_bgm, music_cache, game_icon_queue, available_games,
                        game_selection, game_process,
                    );
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
            draw_texture_ex(
                tex, bx, by, tint,
                DrawTextureParams { dest_size: Some(vec2(bw, bh)), ..Default::default() },
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
fn draw_bokeh(state: &MetroState, s: f32) {
    let t = get_time() as f32;
    let w = screen_width();
    let h = screen_height();
    for b in &state.bokeh {
        let y = (b.y - t * b.speed).rem_euclid(1.15) - 0.075;
        let x = (b.x + (t * b.wobble_hz * std::f32::consts::TAU + b.phase).sin() * b.wobble)
            .rem_euclid(1.0);
        let breath = 0.5 - 0.5 * (t * b.twinkle_hz * std::f32::consts::TAU + b.phase * 1.7).cos();
        let a = b.alpha * breath;
        if a <= 0.003 {
            continue;
        }
        let r = b.r * s;
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
    play_option_enabled: bool,
    copy_logs_option_enabled: bool,
    hero_brands: &[HeroBrandDraw],
    fade_tex: &Texture2D,
    offset_x: f32,
    origin_y: f32,
    animation_state: &AnimationState,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    s: f32,
) {
    let origin_x = ORIGIN_X * s + offset_x;
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
        let r = tile_rect(tile, tab.tiles, origin_x, origin_y, s);
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

        // Cover pass: cart covers sit under the lighting and frame.
        if hero_play {
            for brand in hero_brands {
                draw_hero_brand(brand, rx, ry, rw, rh, true, font_cache, config, s);
            }
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

        if is_selected {
            // Metro selection: crisp white frame (soft corners) with a thin
            // dark seam inside.
            let border = animation_state.get_cursor_color(config);
            draw_focus_frame(rx, ry, rw, rh, 2.5 * s, 1.5 * s, border);
            draw_rectangle_lines(
                rx + 2.5 * s, ry + 2.5 * s,
                rw - 5.0 * s, rh - 5.0 * s,
                1.0 * s,
                Color::new(0.0, 0.0, 0.0, 0.35),
            );
        }

        // The Play hero's mockup furniture — translucent "Play: NAME" bar,
        // media badge, cart icon — comes from the brand layers so it fades
        // with them. An ejected cart's layer still draws over the (now
        // disabled) tile on its way out.
        if hero_play && !hero_brands.is_empty() {
            for brand in hero_brands {
                draw_hero_brand(brand, rx, ry, rw, rh, false, font_cache, config, s);
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
    animation_state: &AnimationState,
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
            let border = animation_state.get_cursor_color(config);
            draw_rectangle_lines(rx, ry, rw, rh, 2.5 * s, border);
            draw_rectangle_lines(
                rx + 2.5 * s, ry + 2.5 * s, rw - 5.0 * s, rh - 5.0 * s,
                1.0 * s, Color::new(0.0, 0.0, 0.0, 0.35),
            );
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

    let w = screen_width();
    let s = scale_factor;
    let t = ease_out(state.anim);
    let current_font = get_current_font(font_cache, config);
    let origin_y = 112.0 * s;

    // Ambient bokeh motes float over the background, under everything else.
    if config.background_particles == "ON" {
        draw_bokeh(state, s);
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

    // --- Panes: the active pane slides in over the previous one ---
    if state.anim < 1.0 && state.prev_tab != state.tab {
        let prev_off = -state.dir * t * w;
        draw_tab_pane(
            &TABS[state.prev_tab], None, 1.0, None, 0.0,
            play_option_enabled, copy_logs_option_enabled,
            &hero_brands, &state.fade_tex,
            prev_off, origin_y, animation_state, font_cache, config, s,
        );
    }
    let active_off = state.dir * (1.0 - t) * w;
    draw_tab_pane(
        &TABS[state.tab], Some(state.tile), state.sel_anim, state.prev_sel, state.press_flash,
        play_option_enabled, copy_logs_option_enabled,
        &hero_brands, &state.fade_tex,
        active_off, origin_y, animation_state, font_cache, config, s,
    );

    // --- Button legend, lower right like the real dash ---
    {
        let legend_size = (FONT_SIZE as f32 * s * 0.8) as u16;
        let label = "Select";
        let dims = measure_text(label, Some(current_font), legend_size, 1.0);
        let radius = 7.0 * s;
        let end_x = screen_width() - 24.0 * s;
        let text_x = end_x - dims.width;
        let cy = 324.0 * s;
        let cx = text_x - 5.0 * s - radius;
        draw_circle(cx, cy, radius, Color::new(0.36, 0.62, 0.10, 1.0));
        draw_circle_lines(cx, cy, radius, 1.2 * s, Color::new(0.0, 0.0, 0.0, 0.35));
        let a_size = (FONT_SIZE as f32 * s * 0.75) as u16;
        let a_dims = measure_text("A", Some(current_font), a_size, 1.0);
        text_with_color(
            font_cache, config, "A",
            cx - a_dims.width / 2.0,
            cy + a_dims.offset_y / 2.0,
            a_size, WHITE,
        );
        text_with_color(
            font_cache, config, label,
            text_x,
            cy + dims.offset_y / 2.0,
            legend_size, Color::new(1.0, 1.0, 1.0, 0.75),
        );
    }

    // --- Tab strip: every tab name in a row, active one big and white ---
    let strip_y = 62.0 * s;
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

    render_ui_overlay(logo_cache, font_cache, config, battery_info, current_time_str, gcc_adapter_poll_rate, scale_factor);

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
