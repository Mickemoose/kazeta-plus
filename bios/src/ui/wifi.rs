use crate::{
    text_with_config_color, get_current_font, DEV_MODE, VideoPlayer,
    audio::SoundEffects,
    config::Config, FONT_SIZE, Screen, BackgroundState, render_background, measure_text, InputState,
    types::BatteryInfo,
    ui::text_with_color,
};
use crate::input::InputSource;
use macroquad::prelude::*;
use std::{
    collections::HashMap,
    process::Command,
    sync::mpsc::{channel, Receiver, Sender},
    thread,
};

// --- On-screen keyboard layout ---
// A 10-column grid, four rows, plus a special row underneath. Two pages
// (letters and symbols); every printable ASCII character is reachable — the
// symbols page skips '@' and '.' because the letters page already carries
// them as spares.
pub const OSK_COLS: usize = 10;

const OSK_ALPHA_LOWER: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl-", "zxcvbnm_.@"];
const OSK_ALPHA_UPPER: [&str; 4] = ["1234567890", "QWERTYUIOP", "ASDFGHJKL-", "ZXCVBNM_.@"];
const OSK_SYMBOLS: [&str; 4] = ["1234567890", "!#$%&'()*+", ",-/:;<=>?[", "\\]^_`{|}\"~"];

/// Number of keys on the special row: page, shift, space, show, connect.
pub const OSK_SPECIALS: usize = 5;

#[derive(Clone, Copy, PartialEq)]
pub enum OskPage {
    Alpha,
    Symbols,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ShiftMode {
    Off,
    Shift, // next letter only
    Caps,  // until turned off
}

#[derive(Debug, Clone)]
pub struct AccessPoint {
    pub ssid: String,
    pub signal_level: u8,
    pub security: String, // "WPA2", "WPA1 WPA2", … or "" for an open network
}

#[derive(PartialEq)]
pub enum WifiScreenState {
    Preparing,
    Scanning,
    List,
    PasswordInput,
    Connecting,
    Connected,
    Error(String),
}

enum WifiMessage {
    PreparationComplete(Result<(), String>),
    ScanComplete(Result<Vec<AccessPoint>, String>),
    ConnectComplete(Result<(), String>),
}

pub struct WifiState {
    pub screen_state: WifiScreenState,
    pub networks: Result<Vec<AccessPoint>, String>,
    pub selected_index: usize,
    pub password_buffer: String,
    /// (row, col): rows 0..=3 are the character grid, row 4 the special row.
    pub osk_coords: (usize, usize),
    pub osk_page: OskPage,
    pub shift: ShiftMode,
    pub show_password: bool,
    /// True while the Wi-Fi screen is the active one; cleared on leave so
    /// re-entering triggers a fresh scan.
    active: bool,
    rx: Receiver<WifiMessage>,
    tx: Sender<WifiMessage>,
}

impl WifiState {
    pub fn new() -> Self {
        let (tx, rx) = channel();

        prepare_wifi_system(tx.clone());

        Self {
            screen_state: WifiScreenState::Preparing,
            networks: Ok(Vec::new()),
            selected_index: 0,
            password_buffer: String::new(),
            osk_coords: (0, 0),
            osk_page: OskPage::Alpha,
            shift: ShiftMode::Off,
            show_password: false,
            active: false,
            rx,
            tx,
        }
    }

    /// The character grid for the current page and shift state.
    pub fn osk_rows(&self) -> [&'static str; 4] {
        match (self.osk_page, self.shift != ShiftMode::Off) {
            (OskPage::Symbols, _) => OSK_SYMBOLS,
            (OskPage::Alpha, true) => OSK_ALPHA_UPPER,
            (OskPage::Alpha, false) => OSK_ALPHA_LOWER,
        }
    }

    /// Face label for special-row key `i`, reflecting live toggle state.
    pub fn special_label(&self, i: usize) -> &'static str {
        match i {
            0 => if self.osk_page == OskPage::Symbols { "abc" } else { "&123" },
            1 => match self.shift {
                ShiftMode::Off => "shift",
                ShiftMode::Shift => "Shift",
                ShiftMode::Caps => "CAPS",
            },
            2 => "space",
            3 => if self.show_password { "hide" } else { "show" },
            4 => "connect",
            _ => "",
        }
    }

    /// Whether special-row key `i` should draw as "toggled on".
    pub fn special_active(&self, i: usize) -> bool {
        match i {
            0 => self.osk_page == OskPage::Symbols,
            1 => self.shift != ShiftMode::Off,
            3 => self.show_password,
            _ => false,
        }
    }

    pub fn selected(&self) -> Option<&AccessPoint> {
        self.networks.as_ref().ok().and_then(|n| n.get(self.selected_index))
    }

    /// One-shot shift releases after a letter; caps lock stays.
    fn consume_shift(&mut self) {
        if self.shift == ShiftMode::Shift {
            self.shift = ShiftMode::Off;
        }
    }

    fn cycle_shift(&mut self) {
        self.shift = match self.shift {
            ShiftMode::Off => ShiftMode::Shift,
            ShiftMode::Shift => ShiftMode::Caps,
            ShiftMode::Caps => ShiftMode::Off,
        };
    }

    /// Scan on a worker thread so the UI keeps animating; the result lands
    /// as a ScanComplete message.
    pub fn scan_networks(&mut self) {
        self.screen_state = WifiScreenState::Scanning;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = run_scan();
            let _ = tx.send(WifiMessage::ScanComplete(result));
        });
    }

    /// Connect on a worker thread; the outcome lands as a ConnectComplete.
    fn attempt_connection(&mut self) {
        let Some(ap) = self.selected() else { return };
        let ssid = ap.ssid.clone();
        let secured = !ap.security.is_empty();
        let password = self.password_buffer.clone();
        self.screen_state = WifiScreenState::Connecting;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = run_connect(&ssid, &password, secured);
            let _ = tx.send(WifiMessage::ConnectComplete(result));
        });
    }
}

fn run_scan() -> Result<Vec<AccessPoint>, String> {
    // The radio switch is persisted NetworkManager state and nothing else on
    // the console ever re-enables it; switched off, scans return an empty
    // list with no error, which reads as "no networks exist". This screen is
    // the user's only way back, so it flips the radio on itself (idempotent,
    // and allowed for the seated session).
    let _ = Command::new("nmcli").args(&["radio", "wifi", "on"]).output();
    let output = Command::new("nmcli")
        .args(&[
            "--terse", "--fields", "SSID,SIGNAL,SECURITY",
            // Without a forced sweep this reads NM's cached list, and weak
            // APs age out of the cache between background scans — which is
            // how a marginal 5GHz network vanishes from the menu.
            "device", "wifi", "list", "--rescan", "yes",
        ])
        .output()
        .map_err(|e| format!("Failed to run nmcli: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut aps: Vec<AccessPoint> = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(':').collect();
        if parts.len() >= 3 {
            let ssid = parts[0];
            let signal_str = parts[1];
            let security = parts[2]; // "" means Open

            if let Ok(signal) = signal_str.parse::<u8>() {
                if !ssid.is_empty() && !aps.iter().any(|a: &AccessPoint| a.ssid == ssid) {
                    aps.push(AccessPoint {
                        ssid: ssid.to_string(),
                        signal_level: signal,
                        security: security.to_string(),
                    });
                }
            }
        }
    }
    aps.sort_by(|a, b| b.signal_level.cmp(&a.signal_level));
    Ok(aps)
}

fn run_connect(ssid: &str, password: &str, secured: bool) -> Result<(), String> {
    // Empty password on a secured network means "use what's saved": the
    // stored profile still holds the key from last time. The old behavior —
    // delete the profile, then join fresh — destroyed that key and then
    // failed with "802-11-wireless-security.psk not given".
    if secured && password.is_empty() {
        let up = Command::new("nmcli")
            .args(&["connection", "up", "id", ssid])
            .output()
            .map_err(|e| format!("Failed to run nmcli: {}", e))?;
        if up.status.success() {
            return Ok(());
        }
        return Err(format!("{} needs a password.", ssid));
    }

    // Fresh credentials: now replacing the old profile is right — one saved
    // with the wrong security settings produces the "key-mgmt property is
    // missing" error.
    let _ = Command::new("nmcli").args(&["connection", "delete", "id", ssid]).output();

    let mut cmd = Command::new("nmcli");
    cmd.arg("device").arg("wifi").arg("connect").arg(ssid);
    if !password.is_empty() {
        cmd.arg("password").arg(password);
    }

    let output = cmd.output().map_err(|e| format!("Failed to run nmcli: {}", e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

pub fn update(
    wifi_state: &mut WifiState,
    input_state: &InputState,
    current_screen: &mut Screen,
    sound_effects: &SoundEffects,
    config: &Config,
) {
    while let Ok(msg) = wifi_state.rx.try_recv() {
        match msg {
            WifiMessage::PreparationComplete(Ok(_)) => wifi_state.scan_networks(),
            WifiMessage::PreparationComplete(Err(e)) => {
                wifi_state.screen_state = WifiScreenState::Error(e);
            }
            WifiMessage::ScanComplete(result) => {
                wifi_state.networks = result;
                wifi_state.selected_index = 0;
                if wifi_state.screen_state == WifiScreenState::Scanning {
                    wifi_state.screen_state = WifiScreenState::List;
                }
            }
            WifiMessage::ConnectComplete(Ok(_)) => {
                sound_effects.play_select(config);
                wifi_state.screen_state = WifiScreenState::Connected;
            }
            WifiMessage::ConnectComplete(Err(e)) => {
                sound_effects.play_reject(config);
                wifi_state.screen_state = WifiScreenState::Error(e);
            }
        }
    }

    // Re-entering the screen sweeps again — the world changed while we were
    // away, and the list should say so.
    if !wifi_state.active {
        wifi_state.active = true;
        if wifi_state.screen_state == WifiScreenState::List {
            wifi_state.scan_networks();
        }
    }

    let leave = |wifi_state: &mut WifiState, current_screen: &mut Screen| {
        wifi_state.active = false;
        *current_screen = crate::ui::extras_return(config);
    };

    match &wifi_state.screen_state {
        WifiScreenState::PasswordInput => {
            update_password_input(wifi_state, input_state, sound_effects, config);
        }
        WifiScreenState::List => {
            if input_state.back {
                sound_effects.play_back(config);
                leave(wifi_state, current_screen);
                return;
            }
            // Shoulder rescan, same gesture as the Bluetooth screen.
            if input_state.next {
                sound_effects.play_select(config);
                wifi_state.scan_networks();
                return;
            }
            if let Ok(networks) = &wifi_state.networks {
                if networks.is_empty() {
                    return;
                }
                if input_state.down && wifi_state.selected_index < networks.len() - 1 {
                    wifi_state.selected_index += 1;
                    sound_effects.play_cursor_move(config);
                }
                if input_state.up && wifi_state.selected_index > 0 {
                    wifi_state.selected_index -= 1;
                    sound_effects.play_cursor_move(config);
                }

                if input_state.select {
                    sound_effects.play_select(config);
                    let selected_ap = &networks[wifi_state.selected_index];
                    if selected_ap.security.is_empty() {
                        // Open network: connect straight away.
                        wifi_state.password_buffer.clear();
                        wifi_state.attempt_connection();
                    } else {
                        wifi_state.password_buffer.clear();
                        wifi_state.osk_coords = (1, 0);
                        wifi_state.osk_page = OskPage::Alpha;
                        wifi_state.shift = ShiftMode::Off;
                        wifi_state.show_password = false;
                        // Drop any stale typed characters from other screens.
                        while get_char_pressed().is_some() {}
                        wifi_state.screen_state = WifiScreenState::PasswordInput;
                    }
                }
            } else if input_state.select {
                // Error listing: select retries the scan.
                sound_effects.play_select(config);
                wifi_state.scan_networks();
            }
        }
        WifiScreenState::Connected | WifiScreenState::Error(_) => {
            if input_state.select || input_state.back {
                sound_effects.play_select(config);
                wifi_state.screen_state = WifiScreenState::List;
                wifi_state.scan_networks();
            }
        }
        WifiScreenState::Connecting => {
            // Back walks away from the wait, not from the attempt — the
            // outcome card still lands when nmcli finishes.
            if input_state.back {
                sound_effects.play_back(config);
                wifi_state.screen_state = WifiScreenState::List;
            }
        }
        WifiScreenState::Preparing | WifiScreenState::Scanning => {
            if input_state.back {
                sound_effects.play_back(config);
                leave(wifi_state, current_screen);
            }
        }
    }
}

/// Password entry: a physical keyboard types directly; a pad walks the grid
/// and gets one-press hotkeys — West backspace, North space, LB shift/caps,
/// RB symbols, Start connect, East cancel.
fn update_password_input(
    wifi_state: &mut WifiState,
    input_state: &InputState,
    sound_effects: &SoundEffects,
    config: &Config,
) {
    let kb_backspace = is_key_pressed(KeyCode::Backspace);
    let kb_enter = is_key_pressed(KeyCode::Enter) || is_key_pressed(KeyCode::KpEnter);
    let kb_escape = is_key_pressed(KeyCode::Escape);
    // Pad-only hotkeys: on a keyboard the same flags fire from ordinary
    // letters (X, E, brackets), which must type, not delete.
    let pad = input_state.last_source == InputSource::Pad;

    // --- Physical keyboard: type straight into the buffer ---
    let mut typed = false;
    while let Some(c) = get_char_pressed() {
        if c >= ' ' && c != '\u{7f}' {
            wifi_state.password_buffer.push(c);
            typed = true;
        }
    }
    if typed {
        sound_effects.play_cursor_move(config);
    }
    if kb_backspace {
        if wifi_state.password_buffer.pop().is_some() {
            sound_effects.play_cursor_move(config);
        }
    }

    // --- Leave / submit ---
    if kb_escape || (input_state.back && !kb_backspace) {
        sound_effects.play_back(config);
        wifi_state.password_buffer.clear();
        wifi_state.screen_state = WifiScreenState::List;
        return;
    }
    if kb_enter || input_state.start {
        if wifi_state.password_buffer.is_empty() {
            sound_effects.play_reject(config);
        } else {
            sound_effects.play_select(config);
            wifi_state.attempt_connection();
        }
        return;
    }

    // --- Pad hotkeys ---
    if pad && input_state.secondary {
        if wifi_state.password_buffer.pop().is_some() {
            sound_effects.play_cursor_move(config);
        } else {
            sound_effects.play_reject(config);
        }
    }
    if pad && input_state.tertiary {
        wifi_state.password_buffer.push(' ');
        sound_effects.play_cursor_move(config);
    }
    if pad && input_state.prev {
        wifi_state.cycle_shift();
        sound_effects.play_cursor_move(config);
    }
    if pad && input_state.next {
        wifi_state.osk_page = if wifi_state.osk_page == OskPage::Alpha {
            OskPage::Symbols
        } else {
            OskPage::Alpha
        };
        sound_effects.play_cursor_move(config);
    }

    // --- Grid navigation, wrapping on both axes ---
    let (mut row, mut col) = wifi_state.osk_coords;
    let mut moved = false;
    if input_state.down {
        row = (row + 1) % 5;
        moved = true;
    }
    if input_state.up {
        row = (row + 4) % 5;
        moved = true;
    }
    if moved {
        // Crossing between the 10-wide grid and the 5-wide special row keeps
        // the cursor over the same screen region.
        if row == 4 {
            col = (col / 2).min(OSK_SPECIALS - 1);
        } else if wifi_state.osk_coords.0 == 4 {
            col = (col * 2).min(OSK_COLS - 1);
        }
    }
    let row_len = if row == 4 { OSK_SPECIALS } else { OSK_COLS };
    if input_state.right {
        col = (col + 1) % row_len;
        moved = true;
    }
    if input_state.left {
        col = (col + row_len - 1) % row_len;
        moved = true;
    }
    if moved {
        wifi_state.osk_coords = (row, col.min(row_len - 1));
        sound_effects.play_cursor_move(config);
    }

    // --- Typing from the grid (pad A, but not the keyboard's Enter — that
    // submits above) ---
    if input_state.select && !kb_enter {
        let (row, col) = wifi_state.osk_coords;
        sound_effects.play_select(config);
        if row < 4 {
            if let Some(key) = wifi_state.osk_rows()[row].chars().nth(col) {
                wifi_state.password_buffer.push(key);
                if key.is_alphabetic() {
                    wifi_state.consume_shift();
                }
            }
        } else {
            match col {
                0 => {
                    wifi_state.osk_page = if wifi_state.osk_page == OskPage::Alpha {
                        OskPage::Symbols
                    } else {
                        OskPage::Alpha
                    };
                }
                1 => wifi_state.cycle_shift(),
                2 => wifi_state.password_buffer.push(' '),
                3 => wifi_state.show_password = !wifi_state.show_password,
                4 => {
                    if wifi_state.password_buffer.is_empty() {
                        sound_effects.play_reject(config);
                    } else {
                        wifi_state.attempt_connection();
                    }
                }
                _ => {}
            }
        }
    }
}

pub fn draw(
    wifi_state: &WifiState,
    animation_state: &mut crate::AnimationState,
    logo_cache: &HashMap<String, Texture2D>,
    background_cache: &HashMap<String, Texture2D>,
    video_cache: &mut HashMap<String, VideoPlayer>,
    font_cache: &HashMap<String, Font>,
    config: &Config,
    background_state: &mut BackgroundState,
    battery_info: &Option<BatteryInfo>,
    current_time_str: &str,
    gcc_adapter_poll_rate: &Option<u32>,
    input_state: &InputState,
    scale_factor: f32,
) {
    if config.menu_style == "METRO" {
        crate::ui::metro::draw_wifi(
            wifi_state, logo_cache, background_cache, video_cache, font_cache, config,
            background_state, battery_info, current_time_str, gcc_adapter_poll_rate,
            input_state, scale_factor,
        );
        return;
    }

    render_background(&background_cache, video_cache, &config, background_state);

    let font = get_current_font(font_cache, config);
    let font_size = (FONT_SIZE as f32 * scale_factor) as u16;
    let line_height = font_size as f32 + 10.0 * scale_factor;
    let container_w = screen_width() * 0.8;
    let container_h = screen_height() * 0.7;
    let container_x = (screen_width() - container_w) / 2.0;
    let container_y = (screen_height() - container_h) / 2.0;
    draw_rectangle(container_x, container_y, container_w, container_h, Color::new(0.0, 0.0, 0.0, 0.75));
    let text_x = container_x + 40.0 * scale_factor;

    match &wifi_state.screen_state {
        WifiScreenState::Preparing => {
            let text = "Preparing network services...";
            let text_dims = measure_text(text, Some(font), font_size, 1.0);
            text_with_config_color(font_cache, config, text, screen_width() / 2.0 - text_dims.width / 2.0, screen_height() / 2.0, font_size);
        }
        WifiScreenState::PasswordInput => {
            if let Some(network) = wifi_state.selected() {
                let prompt = format!("Enter password for \"{}\":", network.ssid);
                text_with_config_color(font_cache, config, &prompt, text_x, container_y + 40.0 * scale_factor, font_size);

                let password_display: String = if wifi_state.show_password {
                    wifi_state.password_buffer.clone()
                } else {
                    wifi_state.password_buffer.chars().map(|_| '*').collect()
                };

                let input_box_y = container_y + 60.0 * scale_factor + 10.0;
                let input_box_height = line_height * 0.8;
                let input_text_font_size = (font_size as f32 * 0.9) as u16;

                draw_rectangle(text_x, input_box_y, container_w - 80.0 * scale_factor, input_box_height, BLACK);
                let text_y_inside_box = input_box_y + (input_box_height / 2.0) + (input_text_font_size as f32 / 2.5);
                draw_text_ex(&password_display, text_x + 10.0 * scale_factor, text_y_inside_box, TextParams { font: Some(font), font_size: input_text_font_size, color: WHITE, ..Default::default() });

                // Dynamic sizing so the 10-column grid fits 4:3 as well.
                let base_osk_size = font_size;
                let base_spacing = base_osk_size as f32 * 1.7;
                let available_width = container_w - 80.0 * scale_factor;
                let needed_width = OSK_COLS as f32 * base_spacing;
                let (osk_font_size, key_spacing) = if needed_width > available_width {
                    let new_spacing = available_width / OSK_COLS as f32;
                    ((new_spacing / 1.7) as u16, new_spacing)
                } else {
                    (base_osk_size, base_spacing)
                };

                let osk_start_y = input_box_y + input_box_height + line_height * 1.2;
                let grid_w = OSK_COLS as f32 * key_spacing;
                let grid_x = container_x + (container_w - grid_w) / 2.0;

                let cursor_color = animation_state.get_cursor_color(config);
                let cursor_scale = animation_state.get_cursor_scale();
                let line_thickness = 4.0 * cursor_scale;
                let rows = wifi_state.osk_rows();

                for (r, row_str) in rows.iter().enumerate() {
                    for (c, key) in row_str.chars().enumerate() {
                        let key_str = key.to_string();
                        let text_dims = measure_text(&key_str, Some(font), osk_font_size, 1.0);
                        let cell_x = grid_x + (c as f32 * key_spacing);
                        let text_draw_x = cell_x + (key_spacing - text_dims.width) / 2.0;
                        let key_y = osk_start_y + (r as f32 * key_spacing * 0.8);

                        let is_selected = (r, c) == wifi_state.osk_coords;

                        if is_selected && config.cursor_style == "BOX" {
                            let box_h = osk_font_size as f32 + 10.0;
                            let box_y = key_y - osk_font_size as f32 - 5.0;
                            draw_rectangle_lines(text_draw_x - 5.0, box_y, text_dims.width + 10.0, box_h, line_thickness, cursor_color);
                        }

                        if is_selected && config.cursor_style == "TEXT" {
                            text_with_color(font_cache, config, &key_str, text_draw_x, key_y, osk_font_size, cursor_color);
                        } else {
                            text_with_config_color(font_cache, config, &key_str, text_draw_x, key_y, osk_font_size);
                        }
                    }
                }

                // --- Special keys row ---
                let special_row_y = osk_start_y + (rows.len() as f32 * key_spacing * 0.8) + 20.0;
                let labels: Vec<&str> = (0..OSK_SPECIALS).map(|i| wifi_state.special_label(i)).collect();
                let key_gap = 40.0 * scale_factor;
                let text_width_sum: f32 = labels.iter().map(|k| measure_text(k, Some(font), osk_font_size, 1.0).width).sum();
                let mut total_row_width = text_width_sum + (labels.len() - 1) as f32 * key_gap;
                let actual_key_gap = if total_row_width > available_width {
                    (available_width - text_width_sum) / (labels.len() as f32 - 1.0)
                } else {
                    key_gap
                };
                total_row_width = text_width_sum + (labels.len() - 1) as f32 * actual_key_gap;

                let mut current_key_x = container_x + (container_w - total_row_width) / 2.0;

                for (c, key_str) in labels.iter().enumerate() {
                    let text_dims = measure_text(key_str, Some(font), osk_font_size, 1.0);
                    let is_selected = (4, c) == wifi_state.osk_coords;
                    let is_active = wifi_state.special_active(c);

                    let mut box_color = if is_active { Color::new(0.3, 0.7, 1.0, 1.0) } else { WHITE };

                    if is_selected {
                        box_color = cursor_color;
                        if config.cursor_style == "BOX" {
                            let box_h = osk_font_size as f32 + 10.0;
                            let box_y = special_row_y - osk_font_size as f32 - 5.0;
                            draw_rectangle_lines(current_key_x - 5.0, box_y, text_dims.width + 10.0, box_h, line_thickness, box_color);
                        }
                    } else if is_active {
                        let box_h = osk_font_size as f32 + 10.0;
                        let box_y = special_row_y - osk_font_size as f32 - 5.0;
                        draw_rectangle_lines(current_key_x - 5.0, box_y, text_dims.width + 10.0, box_h, 2.0, box_color);
                    }

                    if is_selected && config.cursor_style == "TEXT" {
                        text_with_color(font_cache, config, key_str, current_key_x, special_row_y, osk_font_size, cursor_color);
                    } else {
                        text_with_config_color(font_cache, config, key_str, current_key_x, special_row_y, osk_font_size);
                    }

                    current_key_x += text_dims.width + actual_key_gap;
                }

                // Hotkey crib sheet along the bottom of the container.
                let hint = if input_state.last_source == InputSource::Pad {
                    "X backspace · Y space · LB shift · RB symbols · Start connect"
                } else {
                    "Type on your keyboard · Enter connects · Esc cancels"
                };
                let hint_size = (font_size as f32 * 0.7) as u16;
                let hd = measure_text(hint, Some(font), hint_size, 1.0);
                text_with_config_color(font_cache, config, hint,
                    container_x + (container_w - hd.width) / 2.0,
                    container_y + container_h - 16.0 * scale_factor, hint_size);
            }
        }
        WifiScreenState::List => {
            text_with_config_color(font_cache, config, "Available Wi-Fi Networks", text_x, container_y + 30.0 * scale_factor, font_size);
            match &wifi_state.networks {
                Ok(networks) => {
                    if networks.is_empty() {
                        text_with_config_color(font_cache, config, "No networks found.", text_x, container_y + 80.0 * scale_factor, font_size);
                    } else {
                        for (i, ap) in networks.iter().take(10).enumerate() {
                            let y_pos = container_y + 80.0 * scale_factor + (i as f32 * line_height * 1.5);

                            if i == wifi_state.selected_index {
                                draw_rectangle(container_x, y_pos - font_size as f32 - 5.0, container_w, line_height, Color::new(1.0, 1.0, 1.0, 0.2));
                            }

                            text_with_config_color(font_cache, config, &ap.ssid, text_x, y_pos, font_size);

                            let signal_text = format!("{}%", ap.signal_level);
                            let signal_dims = measure_text(&signal_text, Some(font), font_size, 1.0);
                            let signal_x = container_x + container_w - signal_dims.width - (40.0 * scale_factor);
                            text_with_config_color(font_cache, config, &signal_text, signal_x, y_pos, font_size);

                            if !ap.security.is_empty() {
                                let lock_text = "🔒";
                                let lock_dims = measure_text(lock_text, Some(font), font_size, 1.0);
                                let lock_x = signal_x - lock_dims.width - (20.0 * scale_factor);
                                text_with_config_color(font_cache, config, lock_text, lock_x, y_pos, font_size);
                            }
                        }
                    }
                }
                Err(e) => {
                    text_with_config_color(font_cache, config, &format!("Error: {}", e), text_x, container_y + 80.0 * scale_factor, font_size);
                }
            }
        }
        WifiScreenState::Connected => {
            let text = "Successfully Connected!";
            let text_dims = measure_text(text, Some(font), font_size, 1.0);
            text_with_config_color(font_cache, config, text, screen_width() / 2.0 - text_dims.width / 2.0, screen_height() / 2.0, font_size);
        }
        WifiScreenState::Error(msg) => {
            text_with_config_color(font_cache, config, "Connection Failed", text_x, container_y + 80.0 * scale_factor, font_size);

            let max_width = container_w - 80.0 * scale_factor;
            let chars_per_line = (max_width / (font_size as f32 * 0.6)) as usize;
            let mut y_offset = container_y + 80.0 * scale_factor + line_height;
            let chars: Vec<char> = msg.chars().collect();
            let mut start = 0;
            while start < chars.len() {
                let end = usize::min(start + chars_per_line.max(1), chars.len());
                let slice: String = chars[start..end].iter().collect();
                text_with_config_color(font_cache, config, &slice, text_x, y_offset, font_size);
                y_offset += line_height;
                start = end;
            }
        }
        _ => {
            let text = match &wifi_state.screen_state {
                WifiScreenState::Scanning => "Scanning...",
                WifiScreenState::Connecting => "Connecting...",
                _ => ""
            };
            let text_dims = measure_text(text, Some(font), font_size, 1.0);
            text_with_config_color(font_cache, config, text, screen_width() / 2.0 - text_dims.width / 2.0, screen_height() / 2.0, font_size);
        }
    }
}

// --- Background Thread Functions ---

fn prepare_wifi_system(tx: Sender<WifiMessage>) {
    thread::spawn(move || {
        let output;

        if DEV_MODE {
            let _ = tx.send(WifiMessage::PreparationComplete(Ok(())));
            return;
        } else {
            output = Command::new("sudo")
            .arg("/usr/bin/kazeta-wifi-setup")
            .output();
        }

        let result = match output {
            Ok(out) => {
                if out.status.success() {
                    Ok(())
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    Err(format!("Setup script failed: {}", stderr.trim()))
                }
            }
            Err(e) => Err(format!("Failed to run setup script: {}", e)),
        };

        let _ = tx.send(WifiMessage::PreparationComplete(result));
    });
}
