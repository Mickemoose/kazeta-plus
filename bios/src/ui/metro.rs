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
    pub hero: bool, // one big tile per tab, drawn 2x2 units
}

pub struct MetroTab {
    pub title: &'static str,
    pub tiles: &'static [MetroTile],
}

pub const TABS: &[MetroTab] = &[
    MetroTab {
        title: "home",
        tiles: &[
            MetroTile { label: "Play", action: BladeAction::Play, hero: true },
            MetroTile { label: "Save Data", action: BladeAction::SaveData, hero: false },
            MetroTile { label: "Session Logs", action: BladeAction::CopyLogs, hero: false },
            MetroTile { label: "Runtimes", action: BladeAction::RuntimeDownloader, hero: false },
        ],
    },
    MetroTab {
        title: "music",
        tiles: &[
            MetroTile { label: "CD Player", action: BladeAction::CdPlayer, hero: true },
        ],
    },
    MetroTab {
        title: "apps",
        tiles: &[
            MetroTile { label: "Themes", action: BladeAction::ThemeDownloader, hero: true },
            MetroTile { label: "Updates", action: BladeAction::UpdateChecker, hero: false },
        ],
    },
    MetroTab {
        title: "settings",
        tiles: &[
            MetroTile { label: "Settings", action: BladeAction::Settings, hero: true },
            MetroTile { label: "Wi-Fi", action: BladeAction::Wifi, hero: false },
            MetroTile { label: "Bluetooth", action: BladeAction::Bluetooth, hero: false },
            MetroTile { label: "About", action: BladeAction::About, hero: false },
        ],
    },
];

const DEFAULT_TAB: usize = 0; // home
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
    // Hover bgm (cartinfo.yaml `bgm:`): loops while the Play hero is selected,
    // fading in on hover and out on unhover.
    bgm_path: Option<PathBuf>,
    bgm_sink: Option<Sink>,
    bgm_vol: f32,
}

const BGM_FADE_TIME: f32 = 0.7; // seconds for a full fade in or out

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

impl MetroState {
    pub fn new() -> Self {
        let badge_sd = Texture2D::from_file_with_format(include_bytes!("../../SDCARD.png"), None);
        badge_sd.set_filter(FilterMode::Linear);
        let badge_disc = Texture2D::from_file_with_format(include_bytes!("../../DISC.png"), None);
        badge_disc.set_filter(FilterMode::Linear);
        Self {
            tab: DEFAULT_TAB, prev_tab: DEFAULT_TAB, anim: 1.0, dir: 1.0, tile: 0,
            cover_tex: None, icon_tex: None, cart_label: None, cart_optical: false,
            badge_sd, badge_disc, cover_key: String::new(),
            bgm_path: None, bgm_sink: None, bgm_vol: 0.0,
        }
    }

    fn stop_bgm(&mut self) {
        if let Some(sink) = self.bgm_sink.take() {
            sink.stop();
        }
        self.bgm_vol = 0.0;
    }

    fn go_tab(&mut self, to: usize, from_right: bool, land_on_last: bool) {
        self.prev_tab = self.tab;
        self.tab = to;
        self.tile = if land_on_last { TABS[to].tiles.len() - 1 } else { 0 };
        self.anim = 0.0;
        self.dir = if from_right { 1.0 } else { -1.0 };
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Visual order == declaration order: hero first, then small tiles filling
/// 2-row columns top-to-bottom. Column index of small tile i (0-based after
/// the hero) is i/2, row is i%2.
fn tile_rect(idx: usize, tiles: &[MetroTile], origin_x: f32, origin_y: f32, s: f32) -> Rect {
    // Sized in the app's 360p design space (scale_factor blows it up).
    // The hero is a wide banner matching the 920x430 cover art spec.
    let unit = 80.0 * s;
    let gap = 5.0 * s;
    let hero_h = unit * 2.0 + gap;
    let hero_w = hero_h * (920.0 / 430.0);

    let has_hero = tiles.first().map(|t| t.hero).unwrap_or(false);
    if idx == 0 && has_hero {
        return Rect::new(origin_x, origin_y, hero_w, hero_h);
    }
    let small_i = if has_hero { idx - 1 } else { idx };
    let col = (small_i / 2) as f32;
    let row = (small_i % 2) as f32;
    let base_x = origin_x + if has_hero { hero_w + gap } else { 0.0 };
    Rect::new(
        base_x + col * (unit + gap),
        origin_y + row * (unit + gap),
        unit,
        unit,
    )
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

    // Refresh the Play hero's cart branding on insert/eject. available_games
    // only fills when the Play screen opens, so key on cart presence — the
    // cartinfo.yaml probe is pure filesystem and works either way.
    let cover_key = format!(
        "{}:{}:{}",
        *play_option_enabled,
        available_games.len(),
        available_games.first().map(|(_, p)| p.display().to_string()).unwrap_or_default(),
    );
    if cover_key != state.cover_key {
        state.cover_key = cover_key;
        state.cover_tex = None;
        state.icon_tex = None;
        state.cart_label = None;
        state.cart_optical = false;
        state.bgm_path = None;
        state.stop_bgm();
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
    }

    state.anim = (state.anim + get_frame_time() / SLIDE_TIME).min(1.0);

    let tiles = TABS[state.tab].tiles;

    // prev/next (bumpers) hop a whole tab.
    if input_state.prev && state.tab > 0 {
        let to = state.tab - 1;
        state.go_tab(to, false, false);
        sound_effects.play_cursor_move(&config);
    }
    if input_state.next && state.tab + 1 < TABS.len() {
        let to = state.tab + 1;
        state.go_tab(to, true, false);
        sound_effects.play_cursor_move(&config);
    }

    // Left/right walk tiles and cross pane edges like the real dash.
    if input_state.left {
        if state.tile > 0 {
            state.tile -= 1;
            sound_effects.play_cursor_move(&config);
        } else if state.tab > 0 {
            let to = state.tab - 1;
            state.go_tab(to, false, true);
            sound_effects.play_cursor_move(&config);
        }
    }
    if input_state.right {
        if state.tile + 1 < tiles.len() {
            state.tile += 1;
            sound_effects.play_cursor_move(&config);
        } else if state.tab + 1 < TABS.len() {
            let to = state.tab + 1;
            state.go_tab(to, true, false);
            sound_effects.play_cursor_move(&config);
        }
    }

    // Up/down move within a small-tile column (hero has no vertical neighbor).
    let tiles = TABS[state.tab].tiles;
    let has_hero = tiles.first().map(|t| t.hero).unwrap_or(false);
    if (input_state.up || input_state.down) && tiles.len() > 1 {
        if !(state.tile == 0 && has_hero) {
            let small_i = if has_hero { state.tile - 1 } else { state.tile };
            let partner = small_i ^ 1; // the other row in this column
            let partner_idx = if has_hero { partner + 1 } else { partner };
            if partner_idx < tiles.len() && partner_idx != state.tile {
                state.tile = partner_idx;
                sound_effects.play_cursor_move(&config);
            }
        }
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

fn draw_tab_pane(
    tab: &MetroTab,
    selected: Option<usize>,
    play_option_enabled: bool,
    copy_logs_option_enabled: bool,
    cover: Option<&Texture2D>,
    cart_icon: Option<&Texture2D>,
    media_badge: &Texture2D,
    cart_label: Option<&str>,
    offset_x: f32,
    origin_y: f32,
    animation_state: &AnimationState,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    s: f32,
) {
    let origin_x = 75.0 * s + offset_x;
    let current_font = get_current_font(font_cache, config);

    for (idx, tile) in tab.tiles.iter().enumerate() {
        let r = tile_rect(idx, tab.tiles, origin_x, origin_y, s);
        let is_selected = selected == Some(idx);
        let is_disabled = match tile.action {
            BladeAction::Play => !play_option_enabled,
            BladeAction::CopyLogs => !copy_logs_option_enabled,
            _ => false,
        };

        // Flat Metro fill: green for the hero, alternating slate for the rest.
        let mut fill = if tile.hero {
            XBOX_GREEN
        } else if idx % 2 == 0 {
            TILE_SLATE
        } else {
            TILE_SLATE_ALT
        };
        if is_disabled {
            fill = Color::new(fill.r * 0.45, fill.g * 0.45, fill.b * 0.45, 1.0);
        }

        // Selected tiles pop: slight scale about the center plus a white frame.
        let (rx, ry, rw, rh) = if is_selected {
            let grow = 3.0 * s;
            (r.x - grow, r.y - grow, r.w + grow * 2.0, r.h + grow * 2.0)
        } else {
            (r.x, r.y, r.w, r.h)
        };

        // Cover-art hero: the cart's cover fills the tile, mockup-style.
        let is_cover_hero =
            tile.hero && tile.action == BladeAction::Play && !is_disabled && cover.is_some();

        if let (true, Some(tex)) = (is_cover_hero, cover) {
            draw_texture_ex(
                tex, rx, ry, WHITE,
                DrawTextureParams {
                    dest_size: Some(vec2(rw, rh)),
                    ..Default::default()
                },
            );
        } else {
            // Subtle vertical sheen: Metro tiles are flat but lit, brighter up top.
            const STRIPS: usize = 8;
            let strip_h = rh / STRIPS as f32;
            for strip in 0..STRIPS {
                let t = strip as f32 / (STRIPS - 1) as f32;
                let b = 1.08 - 0.16 * t;
                let c = Color::new(
                    (fill.r * b).min(1.0),
                    (fill.g * b).min(1.0),
                    (fill.b * b).min(1.0),
                    fill.a,
                );
                draw_rectangle(rx, ry + strip as f32 * strip_h, rw, strip_h + 1.0, c);
            }
        }

        if is_selected {
            // Metro selection: crisp white frame with a thin dark seam inside.
            let border = animation_state.get_cursor_color(config);
            draw_rectangle_lines(rx, ry, rw, rh, 2.5 * s, border);
            draw_rectangle_lines(
                rx + 2.5 * s, ry + 2.5 * s,
                rw - 5.0 * s, rh - 5.0 * s,
                1.0 * s,
                Color::new(0.0, 0.0, 0.0, 0.35),
            );
        }

        // The Play hero with a cart gets the mockup treatment: translucent
        // "Play: NAME" bar along the bottom plus an SD badge top-right.
        if tile.hero && tile.action == BladeAction::Play && !is_disabled {
            let bar_h = 15.0 * s;
            draw_rectangle(
                rx, ry + rh - bar_h, rw, bar_h,
                Color::new(0.0, 0.0, 0.0, 0.62),
            );
            let text = match cart_label {
                Some(name) => format!("Play: {}", name),
                None => "Play".to_string(),
            };
            let font_size = (FONT_SIZE as f32 * s * 0.78) as u16;
            text_with_color(
                font_cache, config, &text,
                rx + 5.0 * s,
                ry + rh - 4.5 * s,
                font_size, WHITE,
            );
            // Media badge (SD card or disc art) top-right.
            let badge_size = 22.0 * s;
            draw_texture_ex(
                media_badge,
                rx + rw - badge_size - 5.0 * s,
                ry + 5.0 * s,
                WHITE,
                DrawTextureParams {
                    dest_size: Some(vec2(badge_size, badge_size)),
                    ..Default::default()
                },
            );

            // Cart icon: bottom-right, poking out over the top of the bar.
            if let Some(icon) = cart_icon {
                let icon_size = 26.0 * s;
                draw_texture_ex(
                    icon,
                    rx + rw - icon_size - 6.0 * s,
                    ry + rh - icon_size - 3.0 * s,
                    WHITE,
                    DrawTextureParams {
                        dest_size: Some(vec2(icon_size, icon_size)),
                        ..Default::default()
                    },
                );
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

    // --- Panes: the active pane slides in over the previous one ---
    let media_badge = if state.cart_optical { &state.badge_disc } else { &state.badge_sd };
    if state.anim < 1.0 && state.prev_tab != state.tab {
        let prev_off = -state.dir * t * w;
        draw_tab_pane(
            &TABS[state.prev_tab], None,
            play_option_enabled, copy_logs_option_enabled,
            state.cover_tex.as_ref(), state.icon_tex.as_ref(), media_badge, state.cart_label.as_deref(),
            prev_off, origin_y, animation_state, font_cache, config, s,
        );
    }
    let active_off = state.dir * (1.0 - t) * w;
    draw_tab_pane(
        &TABS[state.tab], Some(state.tile),
        play_option_enabled, copy_logs_option_enabled,
        state.cover_tex.as_ref(), state.icon_tex.as_ref(), media_badge, state.cart_label.as_deref(),
        active_off, origin_y, animation_state, font_cache, config, s,
    );

    // --- Tab strip: every tab name in a row, active one big and white ---
    let strip_y = 62.0 * s;
    let mut x = 75.0 * s;
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
