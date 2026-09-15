//! Persistent configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// WoW `Logs` directory to watch. Empty = auto-detect at startup.
    pub log_directory: String,
    /// Where recordings and their metadata sidecars are written.
    pub output_directory: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Video bitrate in kbps (CBR).
    pub bitrate_kbps: u32,
    /// Capture desktop + game audio.
    pub record_audio: bool,
    /// Seconds to keep recording after `CHALLENGE_MODE_END` (loot/scoreboard).
    pub stop_delay_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_directory: String::new(),
            output_directory: default_output_dir().to_string_lossy().into_owned(),
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate_kbps: 12_000,
            record_audio: true,
            stop_delay_secs: 5,
        }
    }
}

fn default_output_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WoW Recorder")
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("wow-recorder")
        .join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let mut cfg: Config = std::fs::read(config_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if cfg.log_directory.is_empty() {
            if let Some(dir) = crate::combatlog::detect_log_directory() {
                cfg.log_directory = dir.to_string_lossy().into_owned();
            }
        }
        cfg
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn output_dir(&self) -> PathBuf {
        PathBuf::from(&self.output_directory)
    }
}
