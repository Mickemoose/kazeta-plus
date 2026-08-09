use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf, error::Error};
use crate::MenuPosition;

/// Returns the path to the user's data directory for Kazeta+.
/// This is a public helper function for other modules to use.
pub fn get_user_data_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|path| path.join(".local/share/kazeta-plus"))
}

fn default_menu_style() -> String {
    "LIST".to_string()
}

fn default_background_particles() -> String {
    "OFF".to_string()
}

fn default_screensaver() -> String {
    "NEVER".to_string()
}

fn default_pad_bgm_volume() -> f32 {
    0.35
}

/// Gets the full path to the kazeta.toml configuration file.
fn get_config_path() -> Result<PathBuf, Box<dyn Error>> {
    let mut config_path = get_user_data_dir().ok_or("Could not find user's data directory.")?;
    fs::create_dir_all(&config_path)?; // Create the directory if it doesn't exist
    config_path.push("config.toml");
    Ok(config_path)
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub aspect_ratio: String,
    pub resolution: String,
    pub show_splash_screen: bool,
    pub timezone: String,
    pub wifi: bool,
    pub bluetooth: bool,
    pub autoboot: bool,
    pub bgm_volume: f32,
    /// Hover theme through a USB-docked DualSense's own speaker/coils.
    /// 0.0 disables; wireless pads have no audio path and silently skip.
    /// serde default keeps config.toml files from before this field parsing.
    #[serde(default = "default_pad_bgm_volume")]
    pub pad_bgm_volume: f32,
    pub sfx_volume: f32,
    pub audio_output: String,
    pub theme: String,
    pub menu_position: MenuPosition,
    #[serde(default = "default_menu_style")]
    pub menu_style: String, // "LIST" (classic) or "METRO" (360-style)
    pub font_color: String,
    pub cursor_color: String,
    pub cursor_style: String,
    pub cursor_blink_speed: String,
    pub cursor_transition_speed: String,
    pub background_scroll_speed: String,
    pub color_shift_speed: String,
    // Ambient bokeh motes over the background ("ON"/"OFF"). serde default so
    // config.toml files written before this field existed still parse.
    #[serde(default = "default_background_particles")]
    pub background_particles: String,
    // Idle screensaver timeout ("NEVER" or "<N> MIN"). serde default keeps
    // older config.toml files parsing.
    #[serde(default = "default_screensaver")]
    pub screensaver: String,
    pub bgm_track: Option<String>,
    pub sfx_pack: String,
    pub logo_selection: String,
    pub background_selection: String,
    pub font_selection: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            aspect_ratio: "16:9".to_string(),
            resolution: "640x360".to_string(),
            show_splash_screen: true,
            timezone: "UTC".to_string(),
            wifi: true,
            bluetooth: true,
            autoboot: true,
            bgm_volume: 0.7,
            pad_bgm_volume: 0.5,
            sfx_volume: 0.7,
            audio_output: "Auto".to_string(),
            theme: "Default".to_string(),
            menu_position: MenuPosition::Center,
            menu_style: default_menu_style(),
            font_color: "WHITE".to_string(),
            cursor_color: "WHITE".to_string(),
            cursor_style: "BOX".to_string(),
            cursor_blink_speed: "NORMAL".to_string(),
            cursor_transition_speed: "NORMAL".to_string(),
            background_scroll_speed: "NORMAL".to_string(),
            color_shift_speed: "NORMAL".to_string(),
            background_particles: default_background_particles(),
            screensaver: default_screensaver(),
            bgm_track: None,
            sfx_pack: "Default".to_string(),
            logo_selection: "Kazeta+ (Default)".to_string(),
            background_selection: "Default".to_string(),
            font_selection: "Default".to_string(),
        }
    }
}

impl Config {
    /// Loads the configuration from config.toml, or returns a default if it fails.
    pub fn load() -> Self {
        if let Ok(config_path) = get_config_path() {
            if let Ok(content) = fs::read_to_string(config_path) {
                if let Ok(config) = toml::from_str::<Self>(&content) {
                    return config.migrated();
                }
            }
        }
        Self::default()
    }

    /// Fixes up values that name something this build no longer has. A config
    /// written before the blades dashboard was removed would otherwise fall
    /// through to the classic List, which is not what anyone who chose the
    /// 360-style menu wanted — Metro is what that choice means now.
    fn migrated(mut self) -> Self {
        if self.menu_style == "BLADES" {
            println!("[Info] menu_style BLADES no longer exists; using METRO.");
            self.menu_style = "METRO".to_string();
        }
        self
    }

    /// Saves the current configuration to config.toml.
    pub fn save(&self) {
        if let Ok(config_path) = get_config_path() {
            if let Ok(toml_string) = toml::to_string_pretty(self) {
                let _ = fs::write(config_path, toml_string);
            }
        }
    }

    pub fn delete() -> std::io::Result<()> {
        if let Ok(config_path) = get_config_path() {
            if config_path.exists() {
                println!("[Info] Deleting config file at: {}", config_path.display());
                std::fs::remove_file(config_path)?;
            }
        }
        Ok(())
    }
}
