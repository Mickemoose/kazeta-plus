use crate::GccMessage;

use std::fs;
use std::thread;
use std::time::Duration;
use std::sync::mpsc::Sender;

// The path where the overclocked driver exposes its poll rate
const GCC_POLL_RATE_PATH: &str = "/sys/module/gcadapter_oc/parameters/rate";

// Nintendo's GameCube controller adapter (Wii U / Switch), the device the
// overclock driver exists for.
const GCC_USB_VENDOR: &str = "057e";
const GCC_USB_PRODUCT: &str = "0337";

/// The module parameter file exists whenever gcadapter_oc is loaded, plugged
/// in or not — so also require the adapter hardware itself on the USB bus.
fn adapter_present() -> bool {
    if let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") {
        for entry in entries.flatten() {
            let path = entry.path();
            let vendor = fs::read_to_string(path.join("idVendor")).unwrap_or_default();
            if vendor.trim() == GCC_USB_VENDOR {
                let product = fs::read_to_string(path.join("idProduct")).unwrap_or_default();
                if product.trim() == GCC_USB_PRODUCT {
                    return true;
                }
            }
        }
    }
    false
}

pub fn start_gcc_adapter_polling(tx: Sender<GccMessage>) {
    thread::spawn(move || {
        let mut was_connected = false;
        loop {
            // The file contains the interval in milliseconds (e.g., "1")
            let poll_rate_hz = if adapter_present() {
                fs::read_to_string(GCC_POLL_RATE_PATH)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .filter(|ms| *ms > 0)
                    .map(|ms| 1000 / ms)
            } else {
                None
            };
            match poll_rate_hz {
                Some(hz) => {
                    tx.send(GccMessage::RateUpdate(hz)).unwrap_or_default();
                    was_connected = true;
                }
                None => {
                    if was_connected {
                        tx.send(GccMessage::Disconnected).unwrap_or_default();
                        was_connected = false;
                    }
                }
            }
            // Check every 2 seconds
            thread::sleep(Duration::from_secs(2));
        }
    });
}
