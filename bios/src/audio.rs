use once_cell::sync::Lazy;
use rodio::{
    self, buffer::SamplesBuffer, source::Source, Decoder as RodioDecoder,
    OutputStream, OutputStreamBuilder, Sink,
};
use std::fs::{self, File};
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::collections::{HashSet, HashMap};
use crate::config::{Config, get_user_data_dir};

// --- Rodio Global Audio System ---
pub struct AudioSystem {
    // Based on your error, the builder returns just an OutputStream.
    // We store it here so we can access its .mixer() later.
    pub stream: OutputStream,
}

// [!] Note: If you get an error that AudioSystem cannot be shared between threads
// (Sync trait), we may need to wrap this in a Mutex. For now, we keep it simple.
pub static AUDIO: Lazy<AudioSystem> = Lazy::new(|| {
    let stream = OutputStreamBuilder::open_default_stream()
    .expect("Failed to load audio stream");
    AudioSystem { stream }
});

// --- DualSense pad speaker stream (hover theme through the controller) ---
//
// A USB-docked DualSense is a plain 4-channel audio card: FL/FR feed the
// 3.5mm jack (and the mono speaker, once the kernel's path selection routes
// it — automatic from Linux 6.18), RL/RR drive the rumble voice-coils, which
// are literal speaker drivers and audibly play music — confirmed on this
// hardware by ear. Over Bluetooth Sony's pad audio is a proprietary
// compressed protocol with no Linux support at all, so no sink exists and
// this whole path silently stands down. PipeWire owns the card, so the
// stream is pinned to the pad's sink via the pipewire ALSA bridge and the
// PIPEWIRE_NODE env var read at PCM-open.

use std::sync::Mutex;

/// The pad's output stream, tagged with the PipeWire node id it was opened
/// against. The id is a fresh number on every replug, and it is the only
/// reliable staleness signal: the node NAME is derived from the USB device and
/// so is byte-identical before and after, while the stream itself is pinned to
/// the old node instance and silently plays into a dead end.
pub static PAD_STREAM: Mutex<Option<(String, OutputStream)>> = Mutex::new(None);

/// The pad's PipeWire sink, as (wpctl id, node name) — None when the pad is
/// wireless or absent.
fn find_pad_sink() -> Option<(String, String)> {
    let out = std::process::Command::new("wpctl").arg("status").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut in_sinks = false;
    let mut id = None;
    for line in text.lines() {
        if line.contains("Sinks:") {
            in_sinks = true;
            continue;
        }
        if in_sinks {
            if line.trim_end().ends_with(':') || line.contains("Sources:") {
                break;
            }
            if line.contains("DualSense") {
                id = line
                    .split(|c: char| !c.is_ascii_digit())
                    .find(|s| !s.is_empty())
                    .map(|s| s.to_string());
                break;
            }
        }
    }
    let id = id?;
    let inspect = std::process::Command::new("wpctl").args(["inspect", &id]).output().ok()?;
    let itext = String::from_utf8_lossy(&inspect.stdout).into_owned();
    let name = itext
        .lines()
        .find(|l| l.contains("node.name"))
        .and_then(|l| l.split('"').nth(1))
        .map(|s| s.to_string())?;
    Some((id, name))
}

/// A Sink on the second output stream pinned to the pad, or None when the
/// pad is absent/wireless/unopenable. The stream persists across hovers;
/// failure never panics — absence is the pad's normal state.
pub fn pad_sink_new() -> Option<Sink> {
    // Re-asserted every hover: the pad forgets its audio path on replug.
    crate::dualsense::enable_speaker();
    // Locate the pad BEFORE consulting the cache, so a replug (new node id) or
    // an unplug (no node at all) can retire a stream that is now pointed at a
    // node PipeWire has destroyed.
    let Some((id, node)) = find_pad_sink() else {
        if let Ok(mut guard) = PAD_STREAM.lock() {
            if guard.take().is_some() {
                println!("[PAD_AUDIO] Pad gone; dropped its stream");
            }
        }
        return None;
    };
    {
        let mut guard = PAD_STREAM.lock().ok()?;
        match guard.as_ref() {
            Some((cached_id, stream)) if *cached_id == id => {
                return Some(Sink::connect_new(stream.mixer()));
            }
            Some((cached_id, _)) => {
                println!(
                    "[PAD_AUDIO] Pad re-enumerated (node {} -> {}); rebuilding stream",
                    cached_id, id
                );
                *guard = None; // drop the dead stream before opening the new one
            }
            None => {}
        }
    }
    // The sink ships muted at the server level; wake it once.
    let _ = std::process::Command::new("wpctl").args(["set-mute", &id, "0"]).output();
    let _ = std::process::Command::new("wpctl").args(["set-volume", &id, "1.0"]).output();
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    let device = rodio::cpal::default_host()
        .output_devices()
        .ok()?
        .find(|d| d.name().map(|n| n == "pipewire").unwrap_or(false))?;
    // pipewire-alsa reads PIPEWIRE_NODE at PCM open — the clean way to pin
    // an ALSA-side stream to one sink. Process-global only for the moment
    // around open; nothing else opens streams mid-session.
    std::env::set_var("PIPEWIRE_NODE", &node);
    let opened = OutputStreamBuilder::from_device(device)
        .and_then(|b| b.with_channels(4).with_sample_rate(48000).open_stream());
    std::env::remove_var("PIPEWIRE_NODE");
    let stream = match opened {
        Ok(s) => s,
        Err(e) => {
            println!("[PAD_AUDIO] Could not open pad stream: {}", e);
            return None;
        }
    };
    let mut guard = PAD_STREAM.lock().ok()?;
    *guard = Some((id, stream));
    guard.as_ref().map(|(_, s)| Sink::connect_new(s.mixer()))
}

/// Any source, refolded into the pad's 4-channel frame: nothing to the
/// jack's left, the mono mix toward the jack-right/speaker feed, and a
/// slightly attenuated copy into the voice-coils — the channels this pad
/// audibly plays today.
pub struct PadSource<S: Source<Item = f32>> {
    inner: S,
    frame: [f32; 4],
    pos: usize,
}

impl<S: Source<Item = f32>> PadSource<S> {
    pub fn new(inner: S) -> Self {
        PadSource { inner, frame: [0.0; 4], pos: 4 }
    }
}

impl<S: Source<Item = f32>> Iterator for PadSource<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.pos >= 4 {
            let ch = self.inner.channels().max(1) as usize;
            let mut acc = 0.0f32;
            let mut n = 0usize;
            for _ in 0..ch {
                match self.inner.next() {
                    Some(s) => {
                        acc += s;
                        n += 1;
                    }
                    None => break,
                }
            }
            if n == 0 {
                return None;
            }
            let s = acc / n as f32;
            // FR carries the music to the real speaker (path-selected by
            // dualsense::enable_speaker); the coils get a whisper of body.
            self.frame = [0.0, s, s * 0.4, s * 0.4];
            self.pos = 0;
        }
        let v = self.frame[self.pos];
        self.pos += 1;
        Some(v)
    }
}

impl<S: Source<Item = f32>> Source for PadSource<S> {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        4
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<std::time::Duration> {
        None
    }
}

// --- Helper functions for loading audio into rodio buffers ---

pub fn load_sound_from_bytes(bytes: &[u8]) -> SamplesBuffer {
    let owned = bytes.to_vec().into_boxed_slice();
    let cursor = Cursor::new(owned);               // Cursor<Box<[u8]>> is 'static
    let decoder = rodio::Decoder::new(cursor).unwrap();
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    let samples: Vec<f32> = decoder.collect();
    SamplesBuffer::new(channels, sample_rate, samples)
}

pub fn load_from_file(path: &Path) -> Result<SamplesBuffer, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let decoder = RodioDecoder::new(reader)?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    let samples: Vec<f32> = decoder.collect();
    Ok(SamplesBuffer::new(channels, sample_rate, samples))
}

// --- SoundEffects Struct and Impl ---

#[derive(Clone)]
pub struct SoundEffects {
    pub cursor_move: SamplesBuffer,
    pub select: SamplesBuffer,
    pub reject: SamplesBuffer,
    pub back: SamplesBuffer,
    // Optional pack extras with no baked-in default: game-launch flourish
    // (launch.wav) and toast pop (toast.wav). Silent when a pack lacks them.
    pub launch: Option<SamplesBuffer>,
    pub toast: Option<SamplesBuffer>,
}

impl SoundEffects {
    pub fn load(pack_name: &str) -> Self {
        let default_move = load_sound_from_bytes(include_bytes!("../move.wav"));
        let default_select = load_sound_from_bytes(include_bytes!("../select.wav"));
        let default_reject = load_sound_from_bytes(include_bytes!("../reject.wav"));
        let default_back = load_sound_from_bytes(include_bytes!("../back.wav"));

        if pack_name == "Default" {
            return SoundEffects {
                cursor_move: default_move,
                select: default_select,
                reject: default_reject,
                back: default_back,
                launch: None,
                toast: None,
            };
        }

        let system_pack_path = format!("../sfx/{}", pack_name);
        let user_pack_path = find_sfx_pack_path(pack_name);

        fn load_one_sfx(
            name: &str,
            user_path_base: &Option<PathBuf>,
            system_path_base: &str,
            fallback: &SamplesBuffer,
        ) -> SamplesBuffer {
            if let Some(base) = user_path_base {
                if let Ok(sound) = load_from_file(&base.join(name)) {
                    return sound;
                }
            }
            let system_path = Path::new(system_path_base).join(name);
            if let Ok(sound) = load_from_file(&system_path) {
                return sound;
            }
            fallback.clone()
        }

        fn load_optional_sfx(
            name: &str,
            user_path_base: &Option<PathBuf>,
            system_path_base: &str,
        ) -> Option<SamplesBuffer> {
            if let Some(base) = user_path_base {
                if let Ok(sound) = load_from_file(&base.join(name)) {
                    return Some(sound);
                }
            }
            load_from_file(&Path::new(system_path_base).join(name)).ok()
        }

        let cursor_move = load_one_sfx("move.wav", &user_pack_path, &system_pack_path, &default_move);
        let select = load_one_sfx("select.wav", &user_pack_path, &system_pack_path, &default_select);
        let reject = load_one_sfx("reject.wav", &user_pack_path, &system_pack_path, &default_reject);
        let back = load_one_sfx("back.wav", &user_pack_path, &system_pack_path, &default_back);
        let launch = load_optional_sfx("launch.wav", &user_pack_path, &system_pack_path);
        let toast = load_optional_sfx("toast.wav", &user_pack_path, &system_pack_path);

        SoundEffects { cursor_move, select, reject, back, launch, toast }
    }

    // [!] FIX: We manually create the Sink using .mixer() instead of .play_once()
    // because play_once requires OutputStreamHandle which you don't have.

    // Every UI sound carries a matching haptic tick — the pad whispers what
    // the speakers say. Strengths sit well under game rumble so the motor
    // reads as texture, not feedback.

    pub fn play_cursor_move(&self, config: &Config) {
        crate::haptics::tick(0.12, 14);
        let source = self.cursor_move.clone().amplify(config.sfx_volume);
        let sink = Sink::connect_new(&AUDIO.stream.mixer());
        sink.append(source);
        sink.detach(); // Fire and forget
    }

    pub fn play_select(&self, config: &Config) {
        crate::haptics::tick(0.35, 22);
        let source = self.select.clone().amplify(config.sfx_volume);
        let sink = Sink::connect_new(&AUDIO.stream.mixer());
        sink.append(source);
        sink.detach();
    }

    pub fn play_reject(&self, config: &Config) {
        crate::haptics::tick(0.55, 34);
        let source = self.reject.clone().amplify(config.sfx_volume);
        let sink = Sink::connect_new(&AUDIO.stream.mixer());
        sink.append(source);
        sink.detach();
    }

    pub fn play_back(&self, config: &Config) {
        crate::haptics::tick(0.20, 16);
        let source = self.back.clone().amplify(config.sfx_volume);
        let sink = Sink::connect_new(&AUDIO.stream.mixer());
        sink.append(source);
        sink.detach();
    }

    /// Game-launch flourish (launch.wav) — silent if the pack has none.
    pub fn play_launch(&self, config: &Config) {
        if let Some(sound) = &self.launch {
            let source = sound.clone().amplify(config.sfx_volume);
            let sink = Sink::connect_new(&AUDIO.stream.mixer());
            sink.append(source);
            sink.detach();
        }
    }

    /// Toast pop (toast.wav) — silent if the pack has none.
    pub fn play_toast(&self, config: &Config) {
        if let Some(sound) = &self.toast {
            let source = sound.clone().amplify(config.sfx_volume);
            let sink = Sink::connect_new(&AUDIO.stream.mixer());
            sink.append(source);
            sink.detach();
        }
    }
}

// --- Filesystem Functions ---
// (This section is unchanged)
pub fn find_sfx_pack_path(pack_name: &str) -> Option<PathBuf> {
    if let Some(themes_dir) = get_user_data_dir().map(|d| d.join("themes")) {
        if let Ok(theme_entries) = fs::read_dir(themes_dir) {
            for theme_entry in theme_entries.flatten() {
                if theme_entry.path().is_dir() {
                    if let Ok(asset_entries) = fs::read_dir(theme_entry.path()) {
                        for asset_entry in asset_entries.flatten() {
                            if asset_entry.path().is_dir() && asset_entry.file_name().to_string_lossy() == pack_name {
                                return Some(asset_entry.path());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

pub fn find_sound_packs() -> Vec<String> {
    let mut packs = HashSet::new();
    packs.insert("Default".to_string());
    let system_sfx_dir = std::path::Path::new("../sfx");
    if let Ok(entries) = fs::read_dir(system_sfx_dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                packs.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    if let Some(user_sfx_dir) = get_user_data_dir().map(|d| d.join("sfx")) {
        if let Ok(entries) = fs::read_dir(user_sfx_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    packs.insert(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    // (Simplified theme searching for brevity - same as your original)
    if let Some(themes_dir) = get_user_data_dir().map(|d| d.join("themes")) {
        if let Ok(theme_entries) = fs::read_dir(themes_dir) {
            for theme_entry in theme_entries.flatten() {
                if theme_entry.path().is_dir() {
                    if let Ok(asset_entries) = fs::read_dir(theme_entry.path()) {
                        for asset_entry in asset_entries.flatten() {
                            if asset_entry.path().is_dir() {
                                packs.insert(asset_entry.file_name().to_string_lossy().into_owned());
                            }
                        }
                    }
                }
            }
        }
    }
    let mut sorted_packs: Vec<String> = packs.into_iter().collect();
    sorted_packs.sort();
    sorted_packs
}

// --- BGM Playback Function ---

pub fn play_new_bgm(
    track_name: &str,
    volume: f32,
    music_cache: &HashMap<String, SamplesBuffer>,
    current_bgm: &mut Option<Sink>,
) {
    if let Some(sink) = current_bgm.take() {
        sink.stop();
    }

    if track_name != "OFF" {
        if let Some(sound_to_play) = music_cache.get(track_name) {
            // [!] FIX: Use Sink::connect_new with the mixer
            let sink = Sink::connect_new(&AUDIO.stream.mixer());

            let source = sound_to_play
            .clone()
            .repeat_infinite()
            .amplify(volume);

            sink.append(source);
            *current_bgm = Some(sink);
        }
    }
}
