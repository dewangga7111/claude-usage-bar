use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    /// Just left of the notification area (clock, tray icons). Windows only.
    Right,
    /// Left edge of the taskbar. Windows only.
    Left,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub position: Position,
    /// Extra horizontal shift in logical pixels, applied after `position`. Windows only.
    pub offset: i32,
    pub poll_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self { position: Position::Right, offset: 0, poll_seconds: 120 }
    }
}

/// `%APPDATA%` on Windows, `~/Library/Application Support` on macOS.
fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = PathBuf::from(std::env::var_os("APPDATA")?);
    #[cfg(not(windows))]
    let base = PathBuf::from(std::env::var_os("HOME")?).join("Library").join("Application Support");
    Some(base.join("claude-usage-bar"))
}

pub fn load() -> Config {
    config_dir()
        .and_then(|dir| std::fs::read(dir.join("config.json")).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

#[cfg_attr(not(windows), allow(dead_code))]
pub fn save(config: &Config) {
    let Some(dir) = config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_vec_pretty(config) {
        let _ = std::fs::write(dir.join("config.json"), json);
    }
}
