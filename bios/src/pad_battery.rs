// Controller battery levels for the overlay: pads report through their hid
// drivers as peripheral power supplies (scope=Device), one node per pad.
// Background-polled so the render loop never touches sysfs.

use std::fs;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

static PAD_BATTERIES: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Battery percentage per connected controller, in stable (device-name) order.
pub fn current() -> Vec<u8> {
    PAD_BATTERIES.lock().map(|v| v.clone()).unwrap_or_default()
}

pub fn start_polling() {
    thread::spawn(|| loop {
        let levels = scan();
        if let Ok(mut slot) = PAD_BATTERIES.lock() {
            *slot = levels;
        }
        thread::sleep(Duration::from_secs(10));
    });
}

fn scan() -> Vec<u8> {
    let mut found: Vec<(String, u8)> = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // Other peripherals (Bluetooth keyboards, mice) also report
            // scope=Device batteries — only gamepad drivers name theirs
            // "controller" (ps-controller-battery-*, nintendo_switch_
            // controller_battery_*, sony_controller_battery_*).
            let looks_pad = name.contains("controller")
                || name.starts_with("xpadneo")
                || name.starts_with("xone");
            let is_battery = fs::read_to_string(path.join("type"))
                .map(|s| s.trim() == "Battery")
                .unwrap_or(false);
            let is_peripheral = fs::read_to_string(path.join("scope"))
                .map(|s| s.trim() == "Device")
                .unwrap_or(false);
            if !is_battery || !is_peripheral || !looks_pad {
                continue;
            }
            // InputPlumber's virtual DualSense registers a duplicate battery
            // node (under /devices/virtual/misc/uhid/) for the same physical
            // pad — skip it or every DualSense shows twice.
            if fs::canonicalize(&path)
                .map(|p| p.to_string_lossy().contains("uhid"))
                .unwrap_or(false)
            {
                continue;
            }
            // Exact percentage when the driver gives one; hid-nintendo can
            // leave `capacity` empty, so fall back to its coarse level.
            let pct = fs::read_to_string(path.join("capacity"))
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
                .or_else(|| {
                    fs::read_to_string(path.join("capacity_level"))
                        .ok()
                        .and_then(|s| match s.trim() {
                            "Full" => Some(100),
                            "High" => Some(75),
                            "Normal" => Some(50),
                            "Low" => Some(25),
                            "Critical" => Some(10),
                            _ => None,
                        })
                });
            if let Some(pct) = pct {
                found.push((name, pct.min(100)));
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.into_iter().map(|(_, pct)| pct).collect()
}
