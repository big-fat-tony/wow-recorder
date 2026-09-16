//! Persistent configuration.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Fallback capture resolution when the WoW window can't be measured.
pub const FALLBACK_RESOLUTION: (u32, u32) = (1920, 1080);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// WoW `Logs` directory to watch. Empty = auto-detect at startup.
    pub log_directory: String,
    /// Where recordings and their metadata sidecars are written.
    pub output_directory: String,
    pub fps: u32,
    /// 0–100, higher = better quality (maps to the encoder's CQP).
    pub quality: u32,
    /// Output downscale: "raw" (match the game), "1440p", "1080p", "720p".
    pub output_mode: String,
    /// Audio capture: "off" | "desktop" (no mic) | "desktop_mic" | "game".
    pub audio_mode: String,
    /// Seconds to keep recording after `CHALLENGE_MODE_END` (loot/scoreboard).
    pub stop_delay_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_directory: String::new(),
            output_directory: default_output_dir().to_string_lossy().into_owned(),
            fps: 30,
            quality: 70,
            output_mode: "1080p".into(),
            audio_mode: "desktop".into(),
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

    /// Encoder CQP value (lower = better). Quality 0→32, 100→16.
    pub fn cqp(&self) -> u32 {
        let q = self.quality.min(100);
        32 - (q * 16 / 100)
    }

    /// Output resolution given the captured input resolution. Never upscales.
    pub fn output_resolution(&self, input: (u32, u32)) -> (u32, u32) {
        let (iw, ih) = input;
        let target_h = match self.output_mode.as_str() {
            "1440p" => 1440,
            "1080p" => 1080,
            "720p" => 720,
            _ => return input, // "raw"
        };
        if ih <= target_h || ih == 0 {
            return input;
        }
        let ow = ((iw as f64 * target_h as f64 / ih as f64).round() as u32) & !1;
        (ow.max(2), target_h)
    }
}
