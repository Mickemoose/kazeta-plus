// Wi-Fi status for the UI overlay: a background thread polls NetworkManager
// for the active SSID so the render loop never blocks on a process spawn.

use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

static WIFI_SSID: Mutex<Option<String>> = Mutex::new(None);

/// SSID of the Wi-Fi network we're currently connected to, if any.
pub fn current_ssid() -> Option<String> {
    WIFI_SSID.lock().ok()?.clone()
}

pub fn start_wifi_ssid_polling() {
    thread::spawn(|| loop {
        let ssid = query_ssid();
        if let Ok(mut slot) = WIFI_SSID.lock() {
            *slot = ssid;
        }
        thread::sleep(Duration::from_secs(5));
    });
}

/// `nmcli -t -f active,ssid dev wifi` prints one `yes:<ssid>` / `no:<ssid>`
/// line per visible network; the `yes` row is the one we're connected to.
/// Terse mode escapes ':' inside values, so unescape before returning.
fn query_ssid() -> Option<String> {
    let output = Command::new("nmcli")
        .args(["-t", "-f", "active,ssid", "dev", "wifi"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("yes:") {
            let ssid = rest.replace("\\:", ":");
            if !ssid.is_empty() {
                return Some(ssid);
            }
        }
    }
    None
}
