//! TOML configuration shared by daemon, settings UI, and backends.
//!
//! Lives at `~/.config/clipvault/config.toml`. Unknown keys are ignored
//! (forward-compatible); missing file → defaults.

use crate::backend::BackendKind;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_max_entries() -> u32 {
    250
}
fn default_max_item_bytes() -> u64 {
    4 * 1024 * 1024 // 4 MiB, like Windows
}
fn default_max_image_store_mb() -> u64 {
    512
}
fn default_true() -> bool {
    true
}
fn default_paste_chord() -> String {
    "ctrl+v".into()
}
fn default_popup_position() -> PopupPosition {
    PopupPosition::Cursor
}
fn default_excluded_apps() -> Vec<String> {
    ["keepassxc", "bitwarden", "1password", "org.keepassxc.KeePassXC"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PopupPosition {
    Cursor,
    Center,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Which clipboard backend to use.
    pub backend: BackendKind,
    /// Master switch: when false, capture pauses but data is kept.
    pub history_enabled: bool,
    /// Also watch the PRIMARY (middle-click) selection. Linux-specific.
    pub monitor_primary: bool,
    pub monitor_images: bool,

    #[serde(default = "default_max_entries")]
    pub max_entries: u32,
    #[serde(default = "default_max_item_bytes")]
    pub max_item_bytes: u64,
    #[serde(default = "default_max_image_store_mb")]
    pub max_image_store_mb: u64,
    /// 0 = keep forever.
    pub max_age_days: u32,

    #[serde(default = "default_true")]
    pub exclude_password_managers: bool,
    #[serde(default = "default_excluded_apps")]
    pub excluded_apps: Vec<String>,
    /// Mask previews until hovered.
    pub conceal_previews: bool,

    #[serde(default = "default_popup_position")]
    pub popup_position: PopupPosition,
    #[serde(default = "default_true")]
    pub auto_paste: bool,
    /// "ctrl+v" (default) or "shift+insert" (works in terminals).
    #[serde(default = "default_paste_chord")]
    pub paste_chord: String,
    /// Delay between refocusing the target window and injecting the chord.
    pub paste_delay_ms: u64,

    /// X11 in-app grab, e.g. "Super+V".
    pub hotkey: String,
    /// Let the DE own the binding and poke us via `clipvault toggle` instead.
    pub use_de_keybinding: bool,

    pub theme: Theme,
    #[serde(default = "default_true")]
    pub show_tray: bool,
    #[serde(default = "default_true")]
    pub launch_at_login: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: BackendKind::Auto,
            history_enabled: true,
            monitor_primary: false,
            monitor_images: true,
            max_entries: default_max_entries(),
            max_item_bytes: default_max_item_bytes(),
            max_image_store_mb: default_max_image_store_mb(),
            max_age_days: 0,
            exclude_password_managers: true,
            excluded_apps: default_excluded_apps(),
            conceal_previews: false,
            popup_position: default_popup_position(),
            auto_paste: true,
            paste_chord: default_paste_chord(),
            paste_delay_ms: 80,
            hotkey: "Super+V".into(),
            use_de_keybinding: false,
            theme: Theme::System,
            show_tray: true,
            launch_at_login: true,
        }
    }
}

pub fn project_dirs() -> Result<ProjectDirs, ConfigError> {
    ProjectDirs::from("io", "clipvault", "clipvault")
        .ok_or(ConfigError::NoHome)
}

impl Config {
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(project_dirs()?.config_dir().join("config.toml"))
    }

    pub fn data_dir() -> Result<PathBuf, ConfigError> {
        Ok(project_dirs()?.data_dir().to_path_buf())
    }

    pub fn images_dir() -> Result<PathBuf, ConfigError> {
        Ok(Self::data_dir()?.join("images"))
    }

    pub fn db_path() -> Result<PathBuf, ConfigError> {
        Ok(Self::data_dir()?.join("history.db"))
    }

    /// Load config; missing file → defaults (and create dirs).
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ConfigError::Io(path.clone(), e))?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse(path.clone(), e))
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| ConfigError::Io(dir.into(), e))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::Serialize(e.to_string()))?;
        std::fs::write(&path, text).map_err(|e| ConfigError::Io(path, e))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine home/config directories")]
    NoHome,
    #[error("I/O on {0}: {1}")]
    Io(PathBuf, std::io::Error),
    #[error("parse {0}: {1}")]
    Parse(PathBuf, toml::de::Error),
    #[error("serialize: {0}")]
    Serialize(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.max_entries, 250);
        assert_eq!(back.max_item_bytes, 4 * 1024 * 1024);
        assert_eq!(back.paste_chord, "ctrl+v");
        assert!(matches!(back.popup_position, PopupPosition::Cursor));
        assert!(back.excluded_apps.iter().any(|a| a == "keepassxc"));
    }

    #[test]
    fn empty_file_gives_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.max_entries, 250);
        assert!(cfg.history_enabled);
    }

    #[test]
    fn unknown_keys_ignored() {
        let cfg: Config = toml::from_str("future_feature = true\nmax_entries = 50").unwrap();
        assert_eq!(cfg.max_entries, 50);
    }

    #[test]
    fn partial_override_keeps_defaults() {
        let cfg: Config = toml::from_str("monitor_primary = true\ntheme = \"dark\"").unwrap();
        assert!(cfg.monitor_primary);
        assert!(matches!(cfg.theme, Theme::Dark));
        assert_eq!(cfg.paste_delay_ms, 80);
    }
}
