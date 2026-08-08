// One vendor output report to a USB-docked DualSense: route audio to the
// internal speaker and raise its amp — without this the pad's real speaker
// stays silent (path defaults to headphones, amp near zero) and only the
// whisper-quiet rumble coils play. Byte layout taken from dualsensectl and
// validated live on this hardware: report 0x02 (USB), valid_flag0 = 0x80
// audio-control + 0x20 speaker-volume + 0x10 headphone-volume, audio_flags
// bits 5:4 = 3 = internal speaker, speaker volume capped at 0x64 like the
// reference tool. Bluetooth pads are skipped outright — Sony's BT audio is
// a proprietary compressed protocol with no Linux path at all.

use std::fs;
use std::io::Write;

pub fn enable_speaker() {
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else { return };
    for e in entries.flatten() {
        let uevent = e.path().join("device/uevent");
        let Ok(txt) = fs::read_to_string(&uevent) else { continue };
        if !txt.contains("DualSense") || !txt.contains("HID_ID=0003") {
            continue;
        }
        let node = format!("/dev/{}", e.file_name().to_string_lossy());
        let mut report = [0u8; 63];
        report[0] = 0x02; // USB output report id
        report[1] = 0x80 | 0x20 | 0x10; // audio control + both volume enables
        report[5] = 0x40; // headphone volume, modest
        report[6] = 0x64; // speaker volume — the reference tool's maximum
        report[8] = 0x30; // output path: internal speaker
        match fs::OpenOptions::new().write(true).open(&node) {
            Ok(mut f) => {
                if f.write_all(&report).is_ok() {
                    println!("[PAD_AUDIO] Speaker path enabled via {}", node);
                } else {
                    println!("[PAD_AUDIO] Report write failed on {}", node);
                }
            }
            Err(err) => println!("[PAD_AUDIO] Cannot open {}: {}", node, err),
        }
        return;
    }
}
