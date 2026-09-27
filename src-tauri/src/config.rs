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
    /// Audio capture: "off" | "desktop" (no mic) | "desktop_mic" | "game" |
    /// "tracks" (two streams: 1 = the game, 2 = desktop + microphone).
    pub audio_mode: String,
    /// Microphone boost in dB, applied before the mic is mixed into its track —
    /// headset mics usually land well below the voices coming out of the
    /// desktop. Ignored when the mic isn't recorded.
    pub mic_gain_db: f32,
    /// Desktop level in dB, applied only when the microphone is mixed into the
    /// same track. Windows hands out a desktop mix that already sits at full
    /// scale, so without a pad the sum overflows and is clamped — crackle.
    pub desktop_gain_db: f32,
    /// Paint a black box over the chat, so whispers, party chat and other
    /// people's names do not end up in footage that gets shared. Burned in, so
    /// it is off unless asked for.
    pub cover_chat: bool,
    /// Where that box goes, as percentages of the frame: x, y, width, height.
    /// The defaults match retail's chat in its default place, measured from a
    /// 2560x1440 recording.
    pub cover_rect_pct: [f32; 4],
    /// Seconds to keep recording after `CHALLENGE_MODE_END` (loot/scoreboard).
    pub stop_delay_secs: u64,
    /// Give up on a run whose combat log has stopped growing for this long and
    /// keep what was recorded — an abandoned key is never ended in the log.
    /// With auto-logging (the Warcraft Logs uploader and friends) this is the
    /// *only* signal that a key was left: logging is switched off on the way
    /// out, so no zone change is ever written. A key being played writes to the
    /// log constantly, so silence this long means nobody is playing it. 0 =
    /// wait forever (the game closing or leaving the dungeon still ends it).
    pub abandon_timeout_secs: u64,
    /// Burn the run clock (`m:ss` since `CHALLENGE_MODE_START`) into the video
    /// so a timestamp from an analyzer can be found in the footage.
    pub timer_overlay: bool,
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
            mic_gain_db: 0.0,
            desktop_gain_db: -6.0,
            cover_chat: false,
            cover_rect_pct: [1.0, 75.0, 21.0, 21.0],
            stop_delay_secs: 5,
            abandon_timeout_secs: 180,
            timer_overlay: true,
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

    /// How long the combat log may be silent before an active run is treated
    /// as abandoned. Zero disables the check; anything shorter than a minute
    /// would trip over a wipe and the run back.
    pub fn abandon_timeout(&self) -> std::time::Duration {
        match self.abandon_timeout_secs {
            0 => std::time::Duration::ZERO,
            secs => std::time::Duration::from_secs(secs.max(60)),
        }
    }

    /// The chat cover as fractions of the frame, or `None` when it is off or
    /// has been given nothing to cover.
    pub fn cover(&self) -> Option<(f32, f32, f32, f32)> {
        let [x, y, w, h] = self.cover_rect_pct;
        (self.cover_chat && w > 0.0 && h > 0.0).then(|| (x / 100.0, y / 100.0, w / 100.0, h / 100.0))
    }

    /// Desktop level in dB for a track shared with the microphone; `0.0` when
    /// the desktop has the track to itself and needs no headroom.
    pub fn desktop_gain(&self) -> f32 {
        match self.audio_mode.as_str() {
            "tracks" | "desktop_mic" => self.desktop_gain_db.clamp(-20.0, 10.0),
            _ => 0.0,
        }
    }

    /// Microphone boost in dB, clamped to a range that can't destroy a track.
    pub fn mic_gain(&self) -> f32 {
        self.mic_gain_db.clamp(-20.0, 30.0)
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
