// Bluetooth device screen and its BlueZ agent.
//
// Reconnection used to never work, and it came down to two BlueZ facts:
//
//   * A paired device that is not *trusted* needs a registered agent to
//     authorise every incoming connection. Ours only exists while the BIOS is
//     running — it dies the instant a game launches — so an untrusted pad
//     could never come back on its own, and re-pairing (which routes through
//     the agent) looked like the only cure. Everything we pair is trusted now,
//     and pairings made before this code are healed when the agent starts.
//
//   * Discovery monopolises the radio. Scanning ran for the BIOS's entire
//     lifetime, so the adapter kept missing the page attempts a controller
//     makes when you wake it. Discovery now runs only while this screen is
//     open, and pauses outright around pair/connect.
//
//   * A link is only a gamepad once the SDP browse lands. bluetoothd builds
//     the kernel HID device from a *cached* service record; if the browse
//     after pairing fails, that record never gets cached, and every later
//     connection is refused in userspace ("Could not parse HID SDP record")
//     — the pad "connects" but never lights up, forever. BlueZ never retries
//     the browse by itself, but Device1.Connect() on an unresolved device
//     re-runs it, even over an already-open link. So the agent drives
//     Connect() instead of waiting, and a poll heals any paired device stuck
//     connected-without-services. Once the record is cached, reconnects work
//     with no agent at all — which is what a running game needs.
//
//     One hard limit, verified against BlueZ 5.83 source: bluetoothd's
//     INTERNAL "already browsed" flag is set even when the browse FAILS, it
//     gates every browse path, and nothing over D-Bus clears it — only a
//     bluetoothd restart does (the bond survives on disk). Worse, the D-Bus
//     ServicesResolved property flips true after a FAILED browse, so it
//     cannot be trusted as proof the record landed; the UUID list can (a
//     failed browse leaves the HID UUID out). So: a pairing whose browse
//     failed is detected by the missing HID UUID and repaired by restarting
//     bluetooth.service (sudoers rule in rootfs allows exactly that one
//     command), after which the heal poll genuinely completes the rescue.
//     Without the rule the repair quietly downgrades to "works after the
//     next console reboot".
//
//   * The console cannot wake a sleeping pad. Paging a DualSense that has
//     powered down just returns Host Is Down, and an awake one refuses HID
//     connections it didn't initiate. Hanging up on a pad puts it to sleep —
//     so nothing here disconnects as a "retry", and an unreachable bonded
//     pad gets "press its home button", which is the truth.
//
// Selecting an already-paired device also used to delete the pairing and
// start over. It connects instead; removing is its own explicit action.

use bluer::{
    Adapter, AdapterEvent, Address, Device, DiscoveryFilter, Result, Session,
    agent::{Agent, RequestAuthorization, RequestConfirmation, RequestPasskey, RequestPinCode},
};
use crate::{
    audio::SoundEffects,
    config::Config,
    types::{AnimationState, BackgroundState, BatteryInfo, Screen},
    ui::text_with_color,
    render_background, render_ui_overlay, get_current_font, measure_text, text_with_config_color,
    FONT_SIZE, InputState, DEV_MODE, VideoPlayer,
};
use futures::StreamExt;
use macroquad::prelude::*;
use std::{
    thread,
    collections::HashMap,
};
use tokio::{
    runtime::Runtime,
    sync::mpsc::{unbounded_channel as tokio_channel, UnboundedReceiver as TokioReceiver, UnboundedSender as TokioSender},
    task::JoinHandle,
    time::{sleep, timeout, Duration},
};

// ===================================
// STRUCTS/ENUMS
// ===================================

/// Coarse device family, used to pick an icon and a verb.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum BtKind {
    Gamepad,
    Audio,
    Keyboard,
    Mouse,
    Phone,
    Computer,
    Other,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct BluetoothDevice {
    pub mac_address: String,
    pub name: String,
    /// BlueZ's `Icon` property ("input-gaming", "audio-headset", …).
    pub icon: String,
    pub paired: bool,
    pub connected: bool,
    pub rssi: Option<i16>,
    pub battery: Option<u8>,
}

impl BluetoothDevice {
    pub fn kind(&self) -> BtKind {
        match self.icon.as_str() {
            "input-gaming" => return BtKind::Gamepad,
            "audio-headset" | "audio-headphones" | "audio-card" | "audio-speakers" => {
                return BtKind::Audio
            }
            "input-keyboard" => return BtKind::Keyboard,
            "input-mouse" | "input-tablet" => return BtKind::Mouse,
            "phone" => return BtKind::Phone,
            "computer" => return BtKind::Computer,
            _ => {}
        }
        // No icon: BlueZ only fills it in once it has read the device class,
        // which can lag behind discovery. The name is a decent stand-in.
        let n = self.name.to_lowercase();
        const PAD: [&str; 9] = [
            "controller", "gamepad", "dualsense", "dualshock", "wireless controller",
            "8bitdo", "joy-con", "xbox", "pro controller",
        ];
        const AUDIO: [&str; 6] = ["headset", "headphone", "earbud", "airpod", "speaker", "buds"];
        if PAD.iter().any(|w| n.contains(w)) {
            BtKind::Gamepad
        } else if AUDIO.iter().any(|w| n.contains(w)) {
            BtKind::Audio
        } else if n.contains("keyboard") {
            BtKind::Keyboard
        } else if n.contains("mouse") {
            BtKind::Mouse
        } else {
            BtKind::Other
        }
    }

    /// What pressing Select does to this device, right now.
    pub fn action_verb(&self) -> &'static str {
        if self.connected {
            "Disconnect"
        } else if self.paired {
            "Connect"
        } else {
            "Pair"
        }
    }

    /// 0–4 bars from RSSI. Useful range is roughly -100 dBm to -40 dBm.
    pub fn bars(&self) -> Option<u8> {
        self.rssi.map(|r| (((r as f32 + 100.0) / 15.0).clamp(0.0, 4.0)) as u8)
    }
}

pub enum BluetoothScreenState {
    DeviceList,
    /// A blocking BlueZ call is in flight; `verb` reads as "Pairing with".
    Working { verb: &'static str, name: String },
    /// Outcome card, dismissed by any button or by timing out.
    Outcome { ok: bool, text: String, at: f64 },
    ForgetConfirm(BluetoothDevice),
}

enum BluetoothMessage {
    Devices(Vec<BluetoothDevice>),
    Working(&'static str, String),
    Done(bool, String),
    /// Discovery actually running or not, so the header tells the truth.
    Scanning(bool),
    /// The agent could not start at all — no adapter, no bluetoothd, dev mode.
    Fatal(String),
}

pub struct BluetoothState {
    pub screen_state: BluetoothScreenState,
    /// Paired devices first (connected ahead of the rest), then discovered.
    pub devices: Vec<BluetoothDevice>,
    pub paired_count: usize,
    pub selected_index: usize,
    pub fatal: Option<String>,
    /// The screen is open, so the agent is scanning.
    pub active: bool,
    /// Whether the radio is currently in discovery (it pauses around
    /// operations and stays off after a successful connect).
    pub scanning: bool,
    /// Time of the last list refresh, for the "no devices yet" copy.
    pub opened_at: f64,
    sel_mac: String,
    rx: TokioReceiver<BluetoothMessage>,
    tx_cmd: TokioSender<String>,
}

// ===================================
// IMPLEMENTATIONS
// ===================================

impl BluetoothState {
    pub fn new() -> Self {
        let (tx_msg, rx_msg) = tokio_channel();
        let (tx_cmd, rx_cmd) = tokio_channel();

        if !DEV_MODE {
            manage_bluetooth_agent(tx_msg, rx_cmd);
        } else {
            println!("[DEV_MODE] Bluetooth agent is disabled.");
            let _ = tx_msg.send(BluetoothMessage::Fatal("Disabled in Dev Mode".to_string()));
        }

        Self {
            screen_state: BluetoothScreenState::DeviceList,
            devices: Vec::new(),
            paired_count: 0,
            selected_index: 0,
            fatal: None,
            active: false,
            scanning: false,
            opened_at: 0.0,
            sel_mac: String::new(),
            rx: rx_msg,
            tx_cmd,
        }
    }

    pub fn selected(&self) -> Option<&BluetoothDevice> {
        self.devices.get(self.selected_index)
    }

    /// Sort into display order and keep the cursor on the same device across
    /// refreshes — the list re-sorts itself as things connect, and a cursor
    /// that tracked the index would wander on its own.
    fn apply(&mut self, mut list: Vec<BluetoothDevice>) {
        list.sort_by(|a, b| {
            b.paired
                .cmp(&a.paired)
                .then(b.connected.cmp(&a.connected))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.paired_count = list.iter().filter(|d| d.paired).count();
        self.devices = list;
        if let Some(i) = self.devices.iter().position(|d| d.mac_address == self.sel_mac) {
            self.selected_index = i;
        } else if self.selected_index >= self.devices.len() {
            self.selected_index = self.devices.len().saturating_sub(1);
        }
        self.sel_mac = self
            .devices
            .get(self.selected_index)
            .map(|d| d.mac_address.clone())
            .unwrap_or_default();
    }

    fn remember_selection(&mut self) {
        self.sel_mac = self
            .devices
            .get(self.selected_index)
            .map(|d| d.mac_address.clone())
            .unwrap_or_default();
    }
}

impl Drop for BluetoothState {
    fn drop(&mut self) {
        let _ = self.tx_cmd.send("scan off".to_string());
    }
}

// ===================================
// FUNCTIONS
// ===================================

pub fn update(
    state: &mut BluetoothState,
    input_state: &InputState,
    current_screen: &mut Screen,
    sound_effects: &SoundEffects,
    config: &Config,
) {
    // Entering the screen wakes the agent. Discovery is left off the rest of
    // the time so wireless pads can actually page the adapter.
    if !state.active {
        state.active = true;
        state.opened_at = get_time();
        let _ = state.tx_cmd.send("scan on".to_string());
    }

    while let Ok(msg) = state.rx.try_recv() {
        match msg {
            BluetoothMessage::Devices(list) => {
                // A live device list means the agent recovered from any
                // earlier fatal (boot race, bluetoothd restart).
                state.fatal = None;
                state.apply(list);
            }
            BluetoothMessage::Working(verb, name) => {
                state.screen_state = BluetoothScreenState::Working { verb, name };
            }
            BluetoothMessage::Done(ok, text) => {
                if ok {
                    sound_effects.play_select(config);
                } else {
                    sound_effects.play_reject(config);
                }
                state.screen_state = BluetoothScreenState::Outcome { ok, text, at: get_time() };
            }
            BluetoothMessage::Scanning(on) => state.scanning = on,
            BluetoothMessage::Fatal(e) => state.fatal = Some(e),
        }
    }

    let leave = |state: &mut BluetoothState, current_screen: &mut Screen| {
        let _ = state.tx_cmd.send("scan off".to_string());
        state.active = false;
        state.screen_state = BluetoothScreenState::DeviceList;
        *current_screen = crate::ui::extras_return(config);
    };

    match &state.screen_state {
        BluetoothScreenState::DeviceList => {
            if !state.devices.is_empty() {
                if input_state.down && state.selected_index + 1 < state.devices.len() {
                    state.selected_index += 1;
                    state.remember_selection();
                    sound_effects.play_cursor_move(config);
                }
                if input_state.up && state.selected_index > 0 {
                    state.selected_index -= 1;
                    state.remember_selection();
                    sound_effects.play_cursor_move(config);
                }
                if input_state.select {
                    if let Some(d) = state.devices.get(state.selected_index).cloned() {
                        let (cmd, verb) = if d.connected {
                            ("disconnect", "Disconnecting")
                        } else if d.paired {
                            ("connect", "Connecting to")
                        } else {
                            ("pair", "Pairing with")
                        };
                        sound_effects.play_select(config);
                        let _ = state.tx_cmd.send(format!("{} {}", cmd, d.mac_address));
                        // Show the wait immediately; the agent confirms it a
                        // moment later with the same message.
                        state.screen_state = BluetoothScreenState::Working {
                            verb,
                            name: d.name.clone(),
                        };
                    }
                }
                // Forget sits on the north face, which is the only spare
                // button with a real glyph in every brand's icon set.
                if input_state.tertiary {
                    if let Some(d) = state.devices.get(state.selected_index).cloned() {
                        if d.paired {
                            sound_effects.play_select(config);
                            state.screen_state = BluetoothScreenState::ForgetConfirm(d);
                        } else {
                            sound_effects.play_reject(config);
                        }
                    }
                }
            }
            // Refresh: drop everything BlueZ cached and sweep again, for when
            // a device was put into pairing mode after the screen opened.
            if input_state.next {
                sound_effects.play_select(config);
                state.opened_at = get_time();
                let _ = state.tx_cmd.send("rescan".to_string());
            }
            if input_state.back {
                sound_effects.play_back(config);
                leave(state, current_screen);
            }
        }
        BluetoothScreenState::ForgetConfirm(device) => {
            if input_state.select {
                let mac = device.mac_address.clone();
                let name = device.name.clone();
                sound_effects.play_select(config);
                let _ = state.tx_cmd.send(format!("forget {}", mac));
                state.screen_state = BluetoothScreenState::Working {
                    verb: "Removing",
                    name,
                };
            } else if input_state.back || input_state.tertiary {
                sound_effects.play_back(config);
                state.screen_state = BluetoothScreenState::DeviceList;
            }
        }
        BluetoothScreenState::Outcome { at, .. } => {
            let expired = get_time() - at > 3.0;
            if expired || input_state.select || input_state.back || input_state.tertiary {
                state.screen_state = BluetoothScreenState::DeviceList;
            }
        }
        BluetoothScreenState::Working { .. } => {
            // Back walks away from the wait, not from the operation — BlueZ
            // finishes either way and the outcome card still shows up.
            if input_state.back {
                sound_effects.play_back(config);
                state.screen_state = BluetoothScreenState::DeviceList;
            }
        }
    }
}

pub fn draw(
    state: &BluetoothState,
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
    input_state: &InputState,
    scale_factor: f32,
) {
    if config.menu_style == "METRO" {
        crate::ui::metro::draw_bluetooth(
            state, logo_cache, background_cache, video_cache, font_cache, config,
            background_state, battery_info, current_time_str, gcc_adapter_poll_rate,
            input_state, scale_factor,
        );
        return;
    }

    render_background(background_cache, video_cache, config, background_state);
    draw_rectangle(0.0, 0.0, screen_width(), screen_height(), Color::new(0.0, 0.0, 0.0, 0.5));
    render_ui_overlay(logo_cache, font_cache, config, battery_info, current_time_str, gcc_adapter_poll_rate, scale_factor);

    let font = get_current_font(font_cache, config);
    let font_size = (FONT_SIZE as f32 * scale_factor) as u16;
    let line_height = font_size as f32 * 1.8;

    let center_x = screen_width() / 2.0;
    let center_y = screen_height() / 2.0;
    let centered = |text: &str, y: f32| {
        let dims = measure_text(text, Some(font), font_size, 1.0);
        text_with_config_color(font_cache, config, text, center_x - dims.width / 2.0, y, font_size);
    };

    match &state.screen_state {
        BluetoothScreenState::DeviceList => {
            if let Some(err) = &state.fatal {
                centered(&format!("Bluetooth unavailable: {}", err), center_y);
                return;
            }
            let start_y = 130.0 * scale_factor;
            if state.devices.is_empty() {
                let dots = ".".repeat((get_time() * 2.0) as usize % 4);
                centered(&format!("Scanning for devices{}", dots), center_y);
            } else {
                for (i, device) in state.devices.iter().enumerate() {
                    let y_pos = start_y + (i as f32 * line_height);
                    let label = if device.connected {
                        format!("{} - Connected", device.name)
                    } else if device.paired {
                        format!("{} - Paired", device.name)
                    } else {
                        device.name.clone()
                    };
                    let dims = measure_text(&label, Some(font), font_size, 1.0);
                    let x_pos = center_x - dims.width / 2.0;
                    let is_selected = i == state.selected_index;

                    if is_selected && config.cursor_style == "BOX" {
                        draw_rectangle_lines(
                            x_pos - 20.0,
                            y_pos - font_size as f32 * 1.3,
                            dims.width + 40.0,
                            line_height,
                            8.0,
                            animation_state.get_cursor_color(config),
                        );
                    }
                    if is_selected && config.cursor_style == "TEXT" {
                        let highlight_color = animation_state.get_cursor_color(config);
                        text_with_color(font_cache, config, &label, x_pos, y_pos, font_size, highlight_color);
                    } else {
                        text_with_config_color(font_cache, config, &label, x_pos, y_pos, font_size);
                    }
                }
            }
        }
        BluetoothScreenState::ForgetConfirm(device) => {
            centered(&format!("Remove {}?", device.name), center_y - line_height);
            centered("Select = Yes / Back = No", center_y + line_height);
        }
        BluetoothScreenState::Working { verb, name } => {
            let dots = ".".repeat((get_time() * 2.0) as usize % 4);
            centered(&format!("{} {}{}", verb, name, dots), center_y);
        }
        BluetoothScreenState::Outcome { text, .. } => centered(text, center_y),
    }
}

// --- Background Thread Function ---

/// Best available human name, falling back to the address BlueZ hands out as
/// an alias when a device never advertised one.
async fn friendly_name(device: &Device, addr: Address) -> String {
    let dashed = addr.to_string().replace(':', "-");
    if let Ok(alias) = device.alias().await {
        if !alias.is_empty() && alias != dashed {
            return alias;
        }
    }
    if let Ok(Some(name)) = device.name().await {
        if !name.is_empty() {
            return name;
        }
    }
    addr.to_string()
}

/// Everything BlueZ currently knows about, as the UI wants it. Doubles as the
/// trust repair pass: any paired device missing the trusted flag gets it here,
/// so a pairing made by an older build (or by bluetoothctl) starts working
/// without the user having to do anything.
async fn snapshot(adapter: &Adapter) -> Vec<BluetoothDevice> {
    let mut out = Vec::new();
    let Ok(addresses) = adapter.device_addresses().await else {
        return out;
    };
    for addr in addresses {
        let Ok(device) = adapter.device(addr) else { continue };
        let paired = device.is_paired().await.unwrap_or(false);
        let named = device.name().await.ok().flatten().filter(|n| !n.is_empty());
        let rssi = device.rssi().await.ok().flatten();
        // Nameless strangers are noise, and so are ghosts: BlueZ lists every
        // device it has EVER seen, but a row with no live RSSI isn't out
        // there right now — pairing at it just ends in Page Timeout. Paired
        // devices always stay (they reconnect on their own initiative).
        if !paired && (named.is_none() || rssi.is_none()) {
            continue;
        }
        if paired && !device.is_trusted().await.unwrap_or(true) {
            if device.set_trusted(true).await.is_ok() {
                println!("[BT_AGENT] Trusted {} so it can reconnect on its own.", addr);
            }
        }
        out.push(BluetoothDevice {
            mac_address: addr.to_string(),
            name: friendly_name(&device, addr).await,
            icon: device.icon().await.ok().flatten().unwrap_or_default(),
            paired,
            connected: device.is_connected().await.unwrap_or(false),
            rssi,
            battery: device.battery_percentage().await.ok().flatten(),
        });
    }
    out
}

fn start_scan(adapter: &Adapter, tx_evt: &TokioSender<AdapterEvent>, slot: &mut Option<JoinHandle<()>>) {
    if slot.is_some() {
        return;
    }
    let adapter = adapter.clone();
    let tx_evt = tx_evt.clone();
    *slot = Some(tokio::spawn(async move {
        // Default filter means "auto" transport, which covers classic pads and
        // LE peripherals in one sweep.
        if let Err(e) = adapter.set_discovery_filter(DiscoveryFilter::default()).await {
            eprintln!("[BT_AGENT] Discovery filter refused: {}", e);
        }
        match adapter.discover_devices().await {
            Ok(mut stream) => {
                while let Some(evt) = stream.next().await {
                    if tx_evt.send(evt).is_err() {
                        break;
                    }
                }
            }
            Err(e) => eprintln!("[BT_AGENT] Discovery failed to start: {}", e),
        }
    }));
}

/// Aborting the task drops the discovery stream, and dropping the stream is
/// what tells BlueZ to stop the inquiry.
fn stop_scan(slot: &mut Option<JoinHandle<()>>) {
    if let Some(handle) = slot.take() {
        handle.abort();
    }
}

/// Every paired device stuck in the connected-but-unresolved half-state.
/// That is the poisoned pairing: the pad walked in on its own, the input
/// profile refused it for lack of a cached HID record, and nothing will
/// retry the browse — the pad sits "connected", dark and dead, until it
/// gives up.
async fn find_half_connected(adapter: &Adapter) -> Vec<Device> {
    let mut out = Vec::new();
    let Ok(addresses) = adapter.device_addresses().await else {
        return out;
    };
    for addr in addresses {
        let Ok(device) = adapter.device(addr) else { continue };
        if device.is_paired().await.unwrap_or(false)
            && device.is_connected().await.unwrap_or(false)
            && !device.is_services_resolved().await.unwrap_or(true)
        {
            out.push(device);
        }
    }
    out
}

/// Did the browse actually deliver? ServicesResolved lies (it reads true
/// after a FAILED browse), but a failed browse leaves the input-service UUID
/// out of the device's UUID list — classic HID (1124) or LE HOGP (1812).
async fn hid_record_present(device: &Device) -> bool {
    match device.uuids().await {
        Ok(Some(uuids)) => uuids.iter().any(|u| {
            let s = u.to_string();
            s.starts_with("00001124") || s.starts_with("00001812")
        }),
        // Can't tell — assume the best rather than restart bluetoothd on a
        // read error.
        _ => true,
    }
}

/// The only cure for a same-session failed browse (see the header): restart
/// bluetoothd. Allowed passwordless by the sudoers rule shipped in
/// rootfs/etc/sudoers.d/kazeta-bt; anywhere that rule is missing this fails
/// quietly and the pairing heals on the next console boot instead.
fn restart_bluetoothd() -> bool {
    std::process::Command::new("sudo")
        .args(["-n", "systemctl", "restart", "bluetooth.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `Pair()` on a task that is NEVER cancelled, and just watch the bond
/// flag from outside.
///
/// Two BlueZ facts force this shape. `Pair()` does not return until it has
/// also browsed the device's services, and on this hardware that browse can
/// wedge ("error updating services: Input/output error" in bluetoothd) long
/// after the bond itself landed. And dropping bluer's `pair()` future is what
/// sends CancelPairing — do that mid-handshake and the controller throws away
/// the key it just stored, leaving a bond the host believes in and the pad
/// doesn't: it blinks, gives up, powers off, and can never reconnect. That
/// was the "pairs then turns off and nothing ever connects again" failure.
async fn pair_never_cancel(device: &Device) -> std::result::Result<(), String> {
    let runner = device.clone();
    let mut task = Some(tokio::spawn(async move { runner.pair().await }));
    for _ in 0..70 {
        // 35 s: a pad in pairing mode advertises for about half a minute.
        if device.is_paired().await.unwrap_or(false) {
            return Ok(());
        }
        if task.as_ref().map(|t| t.is_finished()).unwrap_or(false) {
            match task.take().unwrap().await {
                Ok(Ok(())) => return Ok(()),
                // A retry while an earlier attempt's orphaned Pair() is still
                // pending in bluetoothd bounces off with InProgress in
                // milliseconds — but that orphan may still land the bond, so
                // keep watching the flag instead of failing.
                Ok(Err(e)) if e.to_string().contains("rogress") => {}
                Ok(Err(e)) => return Err(e.to_string()),
                Err(e) => return Err(e.to_string()),
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    // Give up on WAITING, but leave the task alive — aborting it is exactly
    // the CancelPairing this function exists to prevent.
    Err("no answer".to_string())
}

/// Returns (succeeded, bluetoothd restart requested). Success decides
/// whether the radio goes back to scanning; a restart request makes the
/// caller tear down and rebuild the whole session.
async fn do_action(adapter: &Adapter, tx: &TokioSender<BluetoothMessage>, verb: &str, mac: &str) -> (bool, bool) {
    let done = |ok: bool, text: String| -> bool {
        let _ = tx.send(BluetoothMessage::Done(ok, text));
        ok
    };
    let Ok(addr) = mac.parse::<Address>() else {
        return (done(false, format!("Not a valid address: {}", mac)), false);
    };
    let Ok(device) = adapter.device(addr) else {
        return (done(false, "That device is no longer around".to_string()), false);
    };
    let name = friendly_name(&device, addr).await;
    println!("[BT_AGENT] {} {} ({})", verb, name, addr);

    match verb {
        "pair" => {
            let _ = tx.send(BluetoothMessage::Working("Pairing with", name.clone()));
            if !device.is_paired().await.unwrap_or(false) {
                if let Err(e) = pair_never_cancel(&device).await {
                    println!("[BT_AGENT] Pairing {} failed: {}", name, e);
                    // Page Timeout = nothing answered the radio at all: the
                    // device left pairing mode (or was never in it).
                    let msg = if e.contains("Page Timeout") || e.contains("no answer") {
                        format!("Couldn't reach {}. Put it in pairing mode and try again.", name)
                    } else {
                        format!("Could not pair {}: {}", name, e)
                    };
                    return (done(false, msg), false);
                }
                println!("[BT_AGENT] Bonded with {}", name);
            }
            // The whole point: trusted devices are allowed back in without an
            // agent, which is the only state a running game leaves us in.
            let _ = device.set_trusted(true).await;
            // The pair flow is still browsing services and auto-connecting on
            // its own; racing it with our Connect() just makes BlueZ refuse
            // one of them. Give that browse a short window — connect_step
            // drives its own retries from there, and never calls a bare link
            // "connected": without resolved services there is no HID profile,
            // no kernel gamepad, no input.
            let _ = tx.send(BluetoothMessage::Working("Connecting to", name.clone()));
            for _ in 0..12 {
                if device.is_services_resolved().await.unwrap_or(false) {
                    break;
                }
                sleep(Duration::from_millis(500)).await;
            }
            let ok = connect_step(&device, tx, &name, true).await;
            // An input device whose HID UUID never arrived is a poisoned
            // pairing — it will "connect" dark forever in this bluetoothd
            // session, whatever ServicesResolved claims (see header). The
            // restart clears bluetoothd's wedged browse state; the heal poll
            // finishes the rescue when the pad walks back in.
            let probe = BluetoothDevice {
                mac_address: mac.to_string(),
                name: name.clone(),
                icon: device.icon().await.ok().flatten().unwrap_or_default(),
                paired: true,
                connected: false,
                rssi: None,
                battery: None,
            };
            let is_input = matches!(probe.kind(), BtKind::Gamepad | BtKind::Keyboard | BtKind::Mouse);
            if is_input && !hid_record_present(&device).await {
                println!(
                    "[BT_AGENT] {} paired but its HID record never arrived; restarting bluetoothd to unwedge the browse.",
                    name
                );
                if restart_bluetoothd() {
                    let _ = tx.send(BluetoothMessage::Done(
                        true,
                        format!("Paired {}. Wait a few seconds, then press its home button.", name),
                    ));
                    return (true, true);
                }
                let _ = tx.send(BluetoothMessage::Done(
                    true,
                    format!("Paired {}, but it needs a console restart before it will connect.", name),
                ));
                return (true, false);
            }
            (ok, false)
        }
        "connect" => {
            let _ = tx.send(BluetoothMessage::Working("Connecting to", name.clone()));
            let _ = device.set_trusted(true).await;
            (connect_step(&device, tx, &name, false).await, false)
        }
        "disconnect" => {
            let _ = tx.send(BluetoothMessage::Working("Disconnecting", name.clone()));
            let ok = match timeout(Duration::from_secs(15), device.disconnect()).await {
                Ok(Ok(())) => done(true, format!("{} disconnected", name)),
                Ok(Err(e)) => done(false, format!("Could not disconnect: {}", e)),
                Err(_) => done(false, "Disconnect timed out".to_string()),
            };
            (ok, false)
        }
        "forget" => {
            let _ = tx.send(BluetoothMessage::Working("Removing", name.clone()));
            let ok = match adapter.remove_device(addr).await {
                Ok(()) => done(true, format!("{} removed", name)),
                Err(e) => done(false, format!("Could not remove {}: {}", name, e)),
            };
            (ok, false)
        }
        _ => (false, false),
    }
}

/// Errors that mean nothing answered the radio: the pad is asleep (or gone),
/// and only its own button can change that.
fn unreachable_err(e: &str) -> bool {
    e.contains("page-timeout")
        || e.contains("create-socket")
        || e.contains("Host is down")
        || e.contains("Page Timeout")
}

/// `bonded`: this connect follows a successful pairing, so a pad that has
/// gone to sleep is still a SUCCESS — the bond is stored and trusted, and one
/// press of its home button finishes the job without the UI's help.
///
/// A link only counts as connected once ServicesResolved lands: the SDP
/// browse is what caches the HID record that becomes the kernel gamepad
/// (profile, LED, input). BlueZ never retries a failed browse on its own —
/// Connect() is the retry, and it works over an already-open link. Never
/// disconnect here: hanging up puts the pad to sleep, unreachable.
async fn connect_step(device: &Device, tx: &TokioSender<BluetoothMessage>, name: &str, bonded: bool) -> bool {
    let done = |ok: bool, text: String| -> bool {
        let _ = tx.send(BluetoothMessage::Done(ok, text));
        ok
    };
    let _ = tx.send(BluetoothMessage::Working("Connecting to", name.to_string()));
    let mut last = String::new();
    for attempt in 0..3 {
        if !device.is_connected().await.unwrap_or(false)
            || !device.is_services_resolved().await.unwrap_or(false)
        {
            match timeout(Duration::from_secs(12), device.connect()).await {
                Ok(Ok(())) => last.clear(),
                Ok(Err(e)) => last = e.to_string(),
                Err(_) => last = "timed out".to_string(),
            }
        }
        for _ in 0..10 {
            if device.is_connected().await.unwrap_or(false)
                && device.is_services_resolved().await.unwrap_or(false)
            {
                return done(true, format!("{} is connected", name));
            }
            sleep(Duration::from_millis(500)).await;
        }
        if last.is_empty() {
            last = "services never resolved".to_string();
        }
        println!("[BT_AGENT] Connect attempt {} to {} incomplete: {}", attempt + 1, name, last);
        if unreachable_err(&last) {
            break;
        }
        sleep(Duration::from_secs(1)).await;
    }
    if device.is_paired().await.unwrap_or(false) && (bonded || unreachable_err(&last)) {
        let text = if bonded {
            format!("Paired {}. Press its home button to finish connecting.", name)
        } else {
            format!("Press the home button on {} — it connects on its own.", name)
        };
        return done(true, text);
    }
    done(false, format!("{} did not finish connecting ({})", name, last))
}

/// Returns whether the caller should rebuild the session: true after a
/// deliberate bluetoothd restart (the daemon we were talking to is gone),
/// false on a normal shutdown. `start_active` resumes discovery for a
/// rebuild that happened while the Bluetooth screen was open.
async fn run_bluetooth_agent(
    tx: TokioSender<BluetoothMessage>,
    rx_cmd: &mut TokioReceiver<String>,
    start_active: bool,
) -> Result<bool> {
    println!("[BT_AGENT] Initializing D-Bus...");
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;

    let agent = Agent {
        request_confirmation: Some(Box::new(|req: RequestConfirmation| {
            println!("[BT_AGENT] Auto-accepting pairing confirmation (Passkey: {})", req.passkey);
            Box::pin(async { Ok(()) })
        })),
        request_passkey: Some(Box::new(|_req: RequestPasskey| {
            println!("[BT_AGENT] Auto-providing default passkey '0000'");
            Box::pin(async { Ok(0000) })
        })),
        request_pin_code: Some(Box::new(|_req: RequestPinCode| {
            println!("[BT_AGENT] Auto-providing default PIN '0000'");
            Box::pin(async { Ok("0000".to_string()) })
        })),
        request_authorization: Some(Box::new(|_req: RequestAuthorization| {
            println!("[BT_AGENT] Auto-authorizing connection");
            Box::pin(async { Ok(()) })
        })),
        ..Default::default()
    };
    let _agent_handle = session.register_agent(agent).await?;
    println!("[BT_AGENT] Agent registered.");

    // bluetoothd is still bringing the adapter up when the BIOS launches at
    // boot, and pokes at it answer Busy for a moment. Transient — retry, and
    // even if powering keeps failing the agent still runs; bluetoothd powers
    // the adapter on its own in most setups.
    for attempt in 1..=10 {
        match adapter.set_powered(true).await {
            Ok(()) => break,
            Err(e) => {
                println!("[BT_AGENT] Power-on attempt {} refused ({}); retrying.", attempt, e);
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
    let _ = adapter.set_pairable(true).await;
    println!("[BT_AGENT] D-Bus ready. Adapter: {}", adapter.name());

    // Heal anything paired by an older build before the UI asks for anything.
    let healed = snapshot(&adapter).await;
    let _ = tx.send(BluetoothMessage::Devices(healed));

    let (tx_evt, mut rx_evt) = tokio_channel::<AdapterEvent>();
    let mut scan: Option<JoinHandle<()>> = None;
    let mut active = start_active;
    let mut want_restart = false;
    let mut heal_tries: HashMap<Address, u8> = HashMap::new();
    let mut heal_pending: Option<Address> = None;
    if active {
        start_scan(&adapter, &tx_evt, &mut scan);
        let _ = tx.send(BluetoothMessage::Scanning(true));
    }
    // The poll always ticks: even with the screen closed it runs the heal
    // pass below, which is pure D-Bus — the radio stays untouched unless a
    // half-connected pad actually needs rescuing.
    let mut poll = Box::pin(sleep(Duration::from_secs(2)));

    loop {
        tokio::select! {
            // Adapter events only mean "something moved" — the poll branch is
            // what rebuilds the list, so an event just pulls it forward.
            Some(_evt) = rx_evt.recv() => {
                if active {
                    poll = Box::pin(sleep(Duration::from_millis(250)));
                }
            }

            Some(cmd) = rx_cmd.recv() => {
                let mut parts = cmd.splitn(2, ' ');
                let verb = parts.next().unwrap_or_default();
                let arg = parts.next().unwrap_or_default().to_string();
                match verb {
                    "scan" => {
                        active = arg == "on";
                        if active {
                            start_scan(&adapter, &tx_evt, &mut scan);
                            poll = Box::pin(sleep(Duration::from_millis(50)));
                        } else {
                            stop_scan(&mut scan);
                            poll = Box::pin(sleep(Duration::from_secs(2)));
                        }
                        let _ = tx.send(BluetoothMessage::Scanning(active));
                        println!("[BT_AGENT] Discovery {}.", if active { "on" } else { "off" });
                    }
                    "rescan" => {
                        // Cycling discovery makes BlueZ re-inquire immediately
                        // instead of coasting on its cache.
                        stop_scan(&mut scan);
                        sleep(Duration::from_millis(300)).await;
                        if active {
                            start_scan(&adapter, &tx_evt, &mut scan);
                            let _ = tx.send(BluetoothMessage::Scanning(true));
                        }
                        poll = Box::pin(sleep(Duration::from_millis(400)));
                    }
                    "pair" | "connect" | "disconnect" | "forget" => {
                        // Inquiry and paging fight over one radio; pausing
                        // discovery is the difference between a pairing that
                        // takes two seconds and one that times out.
                        stop_scan(&mut scan);
                        sleep(Duration::from_millis(200)).await;
                        let (ok, restart) = do_action(&adapter, &tx, verb, arg.trim()).await;
                        if restart {
                            // bluetoothd is gone; everything this session
                            // holds is stale. The outer loop rebuilds it.
                            want_restart = true;
                            break;
                        }
                        // Restarting inquiry right after a pad connects starves
                        // the fresh HID link and drops it seconds later — the
                        // "pairs but never holds" failure. After a successful
                        // pair/connect the radio stays quiet; RB or reopening
                        // the screen starts a new search.
                        let hold_quiet = ok && matches!(verb, "pair" | "connect");
                        if active && !hold_quiet {
                            start_scan(&adapter, &tx_evt, &mut scan);
                            let _ = tx.send(BluetoothMessage::Scanning(true));
                        } else {
                            let _ = tx.send(BluetoothMessage::Scanning(false));
                        }
                        poll = Box::pin(sleep(Duration::from_millis(200)));
                    }
                    _ => {}
                }
            }

            _ = &mut poll => {
                if tx.is_closed() {
                    println!("[BT_AGENT] UI channel closed. Shutting down.");
                    break;
                }
                // Heal pass. Discovery pauses around it for the same reason
                // it pauses around pairing: inquiry starves SDP. After a
                // successful rescue the radio stays quiet so the fresh HID
                // link can settle. Guard rails: a healthy reconnect passes
                // through the half-state for a moment, so act only when it
                // persists across two ticks; and a browse that keeps failing
                // gets three tries, not a hammer — budgets reset once
                // nothing is stuck.
                let stuck = find_half_connected(&adapter).await;
                if stuck.is_empty() {
                    heal_pending = None;
                    heal_tries.clear();
                } else if let Some(device) = stuck
                    .into_iter()
                    .find(|d| heal_tries.get(&d.address()).copied().unwrap_or(0) < 3)
                {
                    let addr = device.address();
                    if heal_pending == Some(addr) {
                        heal_pending = None;
                        *heal_tries.entry(addr).or_insert(0) += 1;
                        println!("[BT_AGENT] {} is connected without services; driving the SDP browse.", addr);
                        stop_scan(&mut scan);
                        let _ = timeout(Duration::from_secs(12), device.connect()).await;
                        let resolved = device.is_services_resolved().await.unwrap_or(false);
                        println!(
                            "[BT_AGENT] Browse for {} {}.",
                            addr,
                            if resolved { "landed — HID record cached, pad is live" } else { "did not land" }
                        );
                        if active && !resolved {
                            start_scan(&adapter, &tx_evt, &mut scan);
                            let _ = tx.send(BluetoothMessage::Scanning(true));
                        } else if active {
                            let _ = tx.send(BluetoothMessage::Scanning(false));
                        }
                    } else {
                        heal_pending = Some(addr);
                    }
                }
                if active {
                    let list = snapshot(&adapter).await;
                    let _ = tx.send(BluetoothMessage::Devices(list));
                }
                poll = Box::pin(sleep(Duration::from_secs(2)));
            }

            else => break,
        }
    }
    stop_scan(&mut scan);
    println!("[BT_AGENT] Exiting run_bluetooth_agent.");
    Ok(want_restart)
}

fn manage_bluetooth_agent(
    tx: TokioSender<BluetoothMessage>,
    rx_cmd: TokioReceiver<String>,
) {
    thread::spawn(move || {
        println!("[BT_AGENT] Starting Bluetooth agent thread...");
        let rt = Runtime::new().expect("Failed to create Tokio runtime");
        let mut rx_cmd = rx_cmd;
        let mut resume_active = false;
        let mut fails = 0u32;
        loop {
            match rt.block_on(run_bluetooth_agent(tx.clone(), &mut rx_cmd, resume_active)) {
                Ok(true) => {
                    // A poisoned pairing just restarted bluetoothd. Give the
                    // daemon a beat, then rebuild against it; the command
                    // channel survives, so anything the UI sent meanwhile
                    // (like "scan off") is processed by the new session.
                    // Restarts only originate from the Bluetooth screen, so
                    // the rebuild resumes discovery.
                    println!("[BT_AGENT] Rebuilding session after bluetoothd restart.");
                    thread::sleep(Duration::from_secs(2));
                    resume_active = true;
                    fails = 0;
                }
                Ok(false) => break,
                Err(e) => {
                    fails += 1;
                    eprintln!("[BT_AGENT] run_bluetooth_agent failed (attempt {}): {}", fails, e);
                    // Boot races (bluetoothd still starting when the BIOS
                    // launches) resolve in seconds; a genuinely missing
                    // adapter doesn't. Tell the UI after a few strikes, but
                    // never stop retrying — the screen un-fatals itself the
                    // moment a run delivers a device list.
                    if fails == 3 {
                        tx.send(BluetoothMessage::Fatal(format!("{}", e))).ok();
                    }
                    if tx.is_closed() {
                        break;
                    }
                    thread::sleep(Duration::from_secs(3));
                }
            }
        }
        println!("[BT_AGENT] Bluetooth agent thread finished.");
    });
}
