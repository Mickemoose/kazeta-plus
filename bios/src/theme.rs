// Make sure you have the right imports and make your structs public
use crate::audio::SoundEffects;
use crate::config::get_user_data_dir;
use macroquad::prelude::*; // for load_string
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;

// This needs to be public so main.rs can see it
#[derive(Deserialize, Debug, Clone)]
pub struct ThemeConfigFile {
    pub menu_position: Option<String>,
    pub menu_style: Option<String>, // "LIST" or "BLADES"
    pub font_color: Option<String>,
    pub cursor_color: Option<String>,
    pub cursor_style: Option<String>,
    pub cursor_blink_speed: Option<String>,
    pub cursor_transition_speed: Option<String>,
    pub background_scroll_speed: Option<String>,
    pub color_shift_speed: Option<String>,
    pub background_particles: Option<String>, // "ON"/"OFF" ambient bokeh motes
    pub sfx_pack: Option<String>,
    pub bgm_track: Option<String>,
    pub logo_selection: Option<String>,
    pub background_selection: Option<String>,
    pub font_selection: Option<String>,
}

// This also needs to be public
#[derive(Clone)]
pub struct Theme {
    pub name: String,
    pub sounds: SoundEffects,
    // Add other pre-loaded assets here if you want
    // pub background: Texture2D,
    pub config: ThemeConfigFile, // Store the parsed config
}

/// Themes bundled with the OS image live here; they get copied into the
/// user's theme dir on startup so a fresh install has them ready to go.
const BUNDLED_THEMES_DIR: &str = "/usr/share/kazeta-plus/themes";

/// One-way seed of bundled themes into the user's theme dir. Files that
/// already exist are never touched, so user customizations always win.
pub fn seed_bundled_themes() {
    let Some(user_dir) = get_user_data_dir().map(|d| d.join("themes")) else {
        return;
    };
    let bundled = std::path::Path::new(BUNDLED_THEMES_DIR);
    if !bundled.is_dir() {
        return;
    }
    if let Err(e) = copy_missing(bundled, &user_dir) {
        println!("[WARN] Could not seed bundled themes: {}", e);
    }
}

fn copy_missing(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_missing(&from, &to)?;
        } else if !to.exists() {
            fs::copy(&from, &to)?;
            println!("[INFO] Seeded bundled theme file {}", to.display());
        }
    }
    Ok(())
}

// LOAD CUSTOM THEMES
pub async fn load_all_themes() -> HashMap<String, Theme> {
    let mut themes = HashMap::new();
    //let default_sfx = SoundEffects::load("Default").await;
    let default_sfx = SoundEffects::load("Default");

    // create a virtual default theme so we don't crash at startup
    let virtual_default_theme = Theme {
        name: "Default".to_string(),
        sounds: default_sfx.clone(), // Use the pre-loaded default sounds
        config: ThemeConfigFile { // Create an empty config, just like from an empty theme.toml
            menu_position: None,
            menu_style: None,
            font_color: None,
            cursor_color: None,
            cursor_style: None,
            cursor_blink_speed: None,
            cursor_transition_speed: None,
            background_scroll_speed: None,
            color_shift_speed: None,
            background_particles: None,
            sfx_pack: None,
            bgm_track: None,
            logo_selection: None,
            background_selection: None,
            font_selection: None,
        },
    };
    // Insert our virtual theme into the map before scanning for others.
    themes.insert("Default".to_string(), virtual_default_theme);

    let themes_dir = match get_user_data_dir() {
        Some(dir) => dir.join("themes"),
        None => return themes,
    };

    // Use synchronous std::fs to list directories. It's simple and efficient here.
    if let Ok(entries) = fs::read_dir(themes_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let theme_name = path.file_name().unwrap().to_string_lossy().into_owned();
                let toml_path = path.join("theme.toml");

                if toml_path.exists() {
                    // Use macroquad's async load_string to read file contents
                    if let Ok(content) = load_string(&toml_path.to_string_lossy()).await {
                        if let Ok(config) = toml::from_str::<ThemeConfigFile>(&content) {
                            let sounds = match &config.sfx_pack {
                                //Some(pack_name) => SoundEffects::load(pack_name).await,
                                Some(pack_name) => SoundEffects::load(pack_name),
                                None => default_sfx.clone(),
                            };

                            let loaded_theme = Theme {
                                name: theme_name.clone(),
                                sounds,
                                config,
                            };

                            println!("[INFO] Loaded theme '{}'", theme_name);
                            themes.insert(theme_name, loaded_theme);
                        }
                    }
                }
            }
        }
    }
    themes
}
