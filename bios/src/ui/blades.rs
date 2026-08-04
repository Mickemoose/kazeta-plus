// 360-style "blades" main menu: full-height colored panels stacked like tabs.
// Left/right slides between blades, up/down picks an item on the active blade's
// face, select activates it. Enabled via `menu_style = "BLADES"` in config.toml
// or a theme's theme.toml; the classic list stays the default.

use crate::{
    Screen, InputState, render_background, render_ui_overlay, get_current_font, measure_text,
    text_with_config_color, text_disabled, FONT_SIZE, MENU_PADDING, MENU_OPTION_HEIGHT,
    StorageMediaState, VideoPlayer, save,
    audio::SoundEffects,
    config::Config,
    types::{AnimationState, BackgroundState, BatteryInfo},
    ui::text_with_color,
    ui::main_menu::{activate_copy_logs, activate_play, activate_save_data},
};
use macroquad::prelude::*;
use rodio::{buffer::SamplesBuffer, Sink};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    sync::atomic::Ordering,
};

// ===================================
// BLADE DEFINITIONS
// ===================================

#[derive(Clone, Copy, PartialEq)]
pub enum BladeAction {
    SaveData,
    Play,
    CopyLogs,
    Wifi,
    Bluetooth,
    ThemeDownloader,
    RuntimeDownloader,
    CdPlayer,
    UpdateChecker,
    Settings,
    About,
}

pub struct BladeItem {
    pub label: &'static str,
    pub action: BladeAction,
}

pub struct Blade {
    pub title: &'static str,
    pub color: Color,
    pub items: &'static [BladeItem],
}

pub const BLADES: &[Blade] = &[
    Blade {
        title: "MEMORY",
        color: Color::new(0.13, 0.36, 0.66, 1.0), // blue
        items: &[
            BladeItem { label: "SAVE DATA", action: BladeAction::SaveData },
            BladeItem { label: "COPY SESSION LOGS", action: BladeAction::CopyLogs },
        ],
    },
    Blade {
        title: "GAMES",
        color: Color::new(0.36, 0.62, 0.05, 1.0), // the green
        items: &[
            BladeItem { label: "PLAY", action: BladeAction::Play },
        ],
    },
    Blade {
        title: "EXTRAS",
        color: Color::new(0.86, 0.47, 0.07, 1.0), // orange
        items: &[
            BladeItem { label: "CONNECT TO WI-FI", action: BladeAction::Wifi },
            BladeItem { label: "PAIR BLUETOOTH CONTROLLER", action: BladeAction::Bluetooth },
            BladeItem { label: "GET NEW THEMES", action: BladeAction::ThemeDownloader },
            BladeItem { label: "DOWNLOAD RUNTIMES", action: BladeAction::RuntimeDownloader },
            BladeItem { label: "CD PLAYER", action: BladeAction::CdPlayer },
            BladeItem { label: "CHECK FOR UPDATES", action: BladeAction::UpdateChecker },
        ],
    },
    Blade {
        title: "SYSTEM",
        color: Color::new(0.42, 0.44, 0.45, 1.0), // gray
        items: &[
            BladeItem { label: "SETTINGS", action: BladeAction::Settings },
            BladeItem { label: "ABOUT", action: BladeAction::About },
        ],
    },
];

const DEFAULT_BLADE: usize = 1; // GAMES front and center on boot
const SLIDE_TIME: f32 = 0.22; // seconds for a blade switch to settle
const INACTIVE_DIM: f32 = 0.6; // brightness of blades that are tucked away

// ===================================
// STATE
// ===================================

pub struct BladesState {
    pub active: usize, // blade the user is on (slide target)
    pub prev: usize,   // blade we're sliding away from
    pub anim: f32,     // 0..1 slide progress, 1 = settled
    pub item: usize,   // selected item on the active blade
}

impl BladesState {
    pub fn new() -> Self {
        Self { active: DEFAULT_BLADE, prev: DEFAULT_BLADE, anim: 1.0, item: 0 }
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// Left edge of blade `i` when blade `active` is the face. Blades up to the
/// active one stack as tabs on the left; the rest anchor to the right edge.
fn blade_left_edge(i: usize, active: usize, n: usize, tab_w: f32, w: f32) -> f32 {
    if i <= active {
        i as f32 * tab_w
    } else {
        w - (n - i) as f32 * tab_w
    }
}

fn shade(color: Color, brightness: f32) -> Color {
    Color::new(color.r * brightness, color.g * brightness, color.b * brightness, color.a)
}

// ===================================
// UPDATE
// ===================================

pub fn update(
    current_screen: &mut Screen,
    state: &mut BladesState,
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

    // Advance the slide animation.
    state.anim = (state.anim + get_frame_time() / SLIDE_TIME).min(1.0);

    if input_state.left && state.active > 0 {
        state.prev = state.active;
        state.active -= 1;
        state.item = 0;
        state.anim = 0.0;
        sound_effects.play_cursor_move(&config);
    }
    if input_state.right && state.active + 1 < BLADES.len() {
        state.prev = state.active;
        state.active += 1;
        state.item = 0;
        state.anim = 0.0;
        sound_effects.play_cursor_move(&config);
    }

    let items = BLADES[state.active].items;
    if input_state.up && items.len() > 1 {
        state.item = if state.item == 0 { items.len() - 1 } else { state.item - 1 };
        sound_effects.play_cursor_move(&config);
    }
    if input_state.down && items.len() > 1 {
        state.item = (state.item + 1) % items.len();
        sound_effects.play_cursor_move(&config);
    }

    if input_state.select {
        match items[state.item].action {
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

/// One blade panel: a full-height sheet from `x` to the right screen edge with
/// a gently bowed leading edge, drawn as horizontal strips.
fn draw_blade_panel(x: f32, color: Color, s: f32) {
    let w = screen_width();
    let h = screen_height();
    let lean = 14.0 * s;
    const SEGS: usize = 24;
    let seg_h = h / SEGS as f32;

    let mut edge_highlight = color;
    edge_highlight.r = (edge_highlight.r + 0.28).min(1.0);
    edge_highlight.g = (edge_highlight.g + 0.28).min(1.0);
    edge_highlight.b = (edge_highlight.b + 0.28).min(1.0);

    for seg in 0..SEGS {
        let t = (seg as f32 + 0.5) / SEGS as f32;
        // Bow the edge into the blade underneath; the visible sliver pinches at
        // mid-height, which is what sells the "blade" silhouette.
        let ex = x - lean * (t * std::f32::consts::PI).sin();
        let y = seg as f32 * seg_h;
        draw_rectangle(ex, y, (w - ex).max(0.0), seg_h + 1.0, color);
        draw_rectangle(ex, y, 3.0 * s, seg_h + 1.0, edge_highlight);
    }
}

pub fn draw(
    state: &BladesState,
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
    // The blades cover the whole screen, but the theme background still renders
    // first so any future translucency (or a short blade) shows it.
    render_background(background_cache, video_cache, config, background_state);

    let w = screen_width();
    let s = scale_factor;
    let n = BLADES.len();
    let tab_w = 52.0 * s;
    let t = ease_out(state.anim);
    let current_font = get_current_font(font_cache, config);

    // --- Panels, back to front (index order leaves every sliver visible) ---
    let mut face_x = 0.0;
    for i in 0..n {
        let x0 = blade_left_edge(i, state.prev, n, tab_w, w);
        let x1 = blade_left_edge(i, state.active, n, tab_w, w);
        let x = x0 + (x1 - x0) * t;

        let b0 = if i == state.prev { 1.0 } else { INACTIVE_DIM };
        let b1 = if i == state.active { 1.0 } else { INACTIVE_DIM };
        let brightness = b0 + (b1 - b0) * t;

        draw_blade_panel(x, shade(BLADES[i].color, brightness), s);

        if i == state.active {
            face_x = x;
        } else {
            // Vertical label on the tucked-away tab.
            let tab_font_size = (FONT_SIZE as f32 * s * 0.85) as u16;
            draw_text_ex(
                BLADES[i].title,
                x + tab_w * 0.70,
                26.0 * s,
                TextParams {
                    font: Some(current_font),
                    font_size: tab_font_size,
                    rotation: std::f32::consts::FRAC_PI_2,
                    color: Color::new(1.0, 1.0, 1.0, 0.85),
                    ..Default::default()
                },
            );
        }
    }

    // --- Face content ---
    let content_x = face_x + 42.0 * s;
    let title_font_size = (FONT_SIZE as f32 * 1.5 * s) as u16;
    text_with_color(
        font_cache, config, BLADES[state.active].title,
        content_x, 64.0 * s, title_font_size, WHITE,
    );

    let font_size = (FONT_SIZE as f32 * s) as u16;
    let menu_padding = MENU_PADDING * s;
    let item_height = MENU_OPTION_HEIGHT * s * 1.1;
    let items_start_y = 120.0 * s;

    for (idx, item) in BLADES[state.active].items.iter().enumerate() {
        let y = items_start_y + idx as f32 * item_height;
        let is_selected = idx == state.item;
        let is_disabled = match item.action {
            BladeAction::Play => !play_option_enabled,
            BladeAction::CopyLogs => !copy_logs_option_enabled,
            _ => false,
        };

        if is_selected && config.cursor_style == "BOX" {
            let cursor_color = animation_state.get_cursor_color(config);
            let cursor_scale = animation_state.get_cursor_scale();
            let dims = measure_text(item.label, Some(current_font), font_size, 1.0);
            let base_width = dims.width + menu_padding * 2.0;
            let base_height = dims.height + menu_padding * 2.0;
            let scaled_width = base_width * cursor_scale;
            let scaled_height = base_height * cursor_scale;
            let offset_x = (scaled_width - base_width) / 2.0;
            let offset_y = (scaled_height - base_height) / 2.0;

            draw_rectangle_lines(
                content_x - menu_padding - offset_x,
                y - dims.height - menu_padding - offset_y,
                scaled_width,
                scaled_height,
                4.0 * s,
                cursor_color,
            );
        }

        if is_selected && config.cursor_style == "TEXT" {
            let mut highlight_color = animation_state.get_cursor_color(config);
            if is_disabled {
                highlight_color.r *= 0.5;
                highlight_color.g *= 0.5;
                highlight_color.b *= 0.5;
                highlight_color.a = 1.0;
            }
            text_with_color(font_cache, config, item.label, content_x, y, font_size, highlight_color);
        } else if is_disabled {
            text_disabled(font_cache, config, item.label, content_x, y, font_size);
        } else {
            text_with_config_color(font_cache, config, item.label, content_x, y, font_size);
        }
    }

    render_ui_overlay(logo_cache, font_cache, config, battery_info, current_time_str, gcc_adapter_poll_rate, scale_factor);

    // --- Flash message (copy-logs feedback etc), same treatment as the list menu ---
    if let Some(message) = flash_message {
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
