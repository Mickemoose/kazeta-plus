// Player-numbered controller LEDs: player number is join order across ALL
// physical pads (P1 green, P2 blue, P3 red, P4 yellow) — pads without a
// lightbar (Switch Pro, 8BitDo) still occupy a slot, so a DualSense plugged
// in second is player 2 blue. Lightbar-equipped pads get painted, plus the
// matching white player dot. A background thread watches sysfs so it works
// no matter how a pad arrives (boot, hotplug, InputPlumber-managed or raw).
// Write access comes from rootfs/etc/udev/rules.d/60-kazeta-pad-leds.rules.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

const LEDS_DIR: &str = "/sys/class/leds";

// While a game runs it owns the LEDs (it may set its own colors through the
// virtual pad) — the painter pauses instead of fighting it.
static SUSPENDED: AtomicBool = AtomicBool::new(false);

pub fn set_suspended(suspended: bool) {
    SUSPENDED.store(suspended, Ordering::Relaxed);
}

// P1 green, P2 blue, P3 red, P4 yellow. Public so the UI can tint player
// toasts to match the lightbar.
pub const PLAYER_COLORS: [(u8, u8, u8); 4] = [
    (0, 255, 0),
    (0, 0, 255),
    (255, 0, 0),
    (255, 255, 0),
];

/// A controller joined or left; consumed by the dashboard for toasts.
pub struct PadEvent {
    pub slot: usize,        // 0-based player slot at event time
    pub name: String,       // kernel device name
    pub vendor: Option<u16>,
    pub connected: bool,
}

static PAD_EVENTS: Mutex<Vec<PadEvent>> = Mutex::new(Vec::new());

/// Drain pending connect/disconnect events (order preserved).
pub fn take_pad_events() -> Vec<PadEvent> {
    PAD_EVENTS
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

pub fn start_led_painter() {
    thread::spawn(|| {
        let mut last_pads: Vec<(u32, String, Option<u16>)> = Vec::new();
        let mut first_scan = true;
        loop {
            if SUSPENDED.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_secs(2));
                continue;
            }
            let pads = scan_pads();
            if pads != last_pads {
                println!(
                    "[INFO] Controller join order: {:?}",
                    pads.iter().map(|(n, name, _)| (n, name.as_str())).collect::<Vec<_>>()
                );
                // Toast events for changes — but not for the pads already
                // present when the dashboard starts.
                if !first_scan {
                    if let Ok(mut events) = PAD_EVENTS.lock() {
                        for (slot, (num, name, vendor)) in pads.iter().enumerate() {
                            if !last_pads.iter().any(|(n, _, _)| n == num) {
                                events.push(PadEvent {
                                    slot,
                                    name: name.clone(),
                                    vendor: *vendor,
                                    connected: true,
                                });
                            }
                        }
                        for (slot, (num, name, vendor)) in last_pads.iter().enumerate() {
                            if !pads.iter().any(|(n, _, _)| n == num) {
                                events.push(PadEvent {
                                    slot,
                                    name: name.clone(),
                                    vendor: *vendor,
                                    connected: false,
                                });
                            }
                        }
                    }
                }
                last_pads = pads.clone();
                first_scan = false;
            }
            for (slot, (num, _, _)) in pads.into_iter().take(PLAYER_COLORS.len()).enumerate() {
                let rgb = format!("input{}:rgb:indicator", num);
                if fs::metadata(format!("{}/{}", LEDS_DIR, rgb)).is_ok() {
                    // Re-assert every tick: InputPlumber writes the lightbar
                    // through its own hidraw connection, which the kernel
                    // LED sysfs can't see — each brightness write re-sends
                    // the output report so the pad settles on our color.
                    paint(&rgb, slot);
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Physical gamepads in join order (input numbers rise monotonically) with
/// their kernel name and USB vendor id. Skips InputPlumber's virtual mirrors
/// (uhid/uinput devices) and non-pad peripherals, so one controller is one
/// player slot.
fn scan_pads() -> Vec<(u32, String, Option<u16>)> {
    const PAD_WORDS: [&str; 7] = [
        "controller", "gamepad", "8bitdo", "joystick", "joy-con", "xbox", "x-box",
    ];
    const NOT_PAD_WORDS: [&str; 5] = ["motion", "imu", "touchpad", "keyboard", "mouse"];
    let mut pads = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/input") {
        for entry in entries.flatten() {
            let dir = entry.file_name().to_string_lossy().into_owned();
            let Some(num) = dir.strip_prefix("input").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let name = fs::read_to_string(entry.path().join("name"))
                .unwrap_or_default()
                .trim()
                .to_string();
            let lower = name.to_lowercase();
            if !PAD_WORDS.iter().any(|w| lower.contains(w))
                || NOT_PAD_WORDS.iter().any(|w| lower.contains(w))
            {
                continue;
            }
            let virtual_dev = fs::canonicalize(entry.path())
                .map(|p| {
                    let p = p.to_string_lossy().into_owned();
                    p.contains("uhid") || p.contains("/virtual/")
                })
                .unwrap_or(true);
            if virtual_dev {
                continue;
            }
            let vendor = fs::read_to_string(entry.path().join("id/vendor"))
                .ok()
                .and_then(|s| u16::from_str_radix(s.trim(), 16).ok());
            pads.push((num, name, vendor));
        }
    }
    pads.sort_unstable_by_key(|(n, _, _)| *n);
    pads
}

fn paint(rgb_name: &str, slot: usize) {
    let (r, g, b) = PLAYER_COLORS[slot];
    let base = format!("{}/{}", LEDS_DIR, rgb_name);
    let _ = fs::write(format!("{}/multi_intensity", base), format!("{} {} {}", r, g, b));
    let _ = fs::write(format!("{}/brightness", base), "255");

    // Matching white player dot: light dot slot+1, clear the rest.
    let prefix = rgb_name.trim_end_matches(":rgb:indicator");
    for dot in 1..=5 {
        let dot_path = format!("{}/{}:white:player-{}/brightness", LEDS_DIR, prefix, dot);
        let _ = fs::write(dot_path, if dot == slot + 1 { "1" } else { "0" });
    }
}
