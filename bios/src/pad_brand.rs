// Physical controller identity via InputPlumber's D-Bus API. InputPlumber
// hides pads behind virtual devices (a Switch Pro presents as an X-Box 360
// pad), but each CompositeDevice's Name property carries the real hardware
// name — polled here so the UI can pick brand-correct button glyphs.

use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

static COMPOSITE_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Names of every composite device InputPlumber is currently managing.
/// Empty when InputPlumber is idle (then virtual pads don't exist either).
pub fn current() -> Vec<String> {
    COMPOSITE_NAMES.lock().map(|v| v.clone()).unwrap_or_default()
}

pub fn start_polling() {
    thread::spawn(|| loop {
        let names = query_names();
        if let Ok(mut slot) = COMPOSITE_NAMES.lock() {
            *slot = names;
        }
        thread::sleep(Duration::from_secs(4));
    });
}

fn query_names() -> Vec<String> {
    let Ok(tree) = Command::new("busctl")
        .args(["tree", "--list", "org.shadowblip.InputPlumber"])
        .output()
    else {
        return Vec::new();
    };
    let tree = String::from_utf8_lossy(&tree.stdout);
    let mut names = Vec::new();
    for line in tree.lines() {
        let path = line.trim();
        if !path.contains("CompositeDevice") {
            continue;
        }
        let Ok(prop) = Command::new("busctl")
            .args([
                "get-property",
                "org.shadowblip.InputPlumber",
                path,
                "org.shadowblip.Input.CompositeDevice",
                "Name",
            ])
            .output()
        else {
            continue;
        };
        // Output form: s "Sony Interactive Entertainment DualSense ..."
        let prop = String::from_utf8_lossy(&prop.stdout);
        if let Some(name) = prop.split('"').nth(1) {
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names
}
