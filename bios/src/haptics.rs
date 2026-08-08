// Haptic ticks for UI navigation: whisper-strength rumble pulses through
// InputPlumber's ForceFeedback interface, riding the same sound-effect
// moments the ears already get. A worker thread owns the D-Bus chatter
// (busctl subprocesses) so the render loop never blocks, and a burst of
// ticks coalesces into its strongest member — holding a direction repeats
// the cursor fast, and a queue would smear that into one long late drone.

use std::process::Command;
use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

const DEVICE: &str = "/org/shadowblip/InputPlumber/CompositeDevice0";

static TX: OnceLock<Sender<(f64, u64)>> = OnceLock::new();

pub fn init() {
    let (tx, rx) = channel::<(f64, u64)>();
    if TX.set(tx).is_err() {
        return;
    }
    thread::spawn(move || loop {
        let Ok((mut strength, mut ms)) = rx.recv() else { return };
        loop {
            match rx.try_recv() {
                Ok((s, m)) => {
                    if s > strength {
                        strength = s;
                        ms = m;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let _ = Command::new("busctl")
            .args([
                "--system", "call", "org.shadowblip.InputPlumber", DEVICE,
                "org.shadowblip.Output.ForceFeedback", "Rumble", "d",
                &format!("{:.2}", strength),
            ])
            .output();
        thread::sleep(Duration::from_millis(ms));
        let _ = Command::new("busctl")
            .args([
                "--system", "call", "org.shadowblip.InputPlumber", DEVICE,
                "org.shadowblip.Output.ForceFeedback", "Stop",
            ])
            .output();
    });
}

/// Fire-and-forget tick: `strength` 0..1, `ms` pulse length. Silently a
/// no-op before init, without a pad, or on systems with no InputPlumber
/// (the VM) — the busctl call just fails and nobody hears about it.
pub fn tick(strength: f64, ms: u64) {
    if let Some(tx) = TX.get() {
        let _ = tx.send((strength, ms));
    }
}
