//! The recording controller: turns combat-log events into recordings.

use std::path::PathBuf;

use chrono::Utc;
use serde::Serialize;

use crate::combatlog::{ChallengeEnd, ChallengeStart, LogEvent};
use crate::config::Config;
use crate::recorder::Recorder;

/// Written next to each video so tools (wcl-uploader, bdk-analyzer) can align a
/// Warcraft Logs report with the footage later.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingMetadata {
    pub video_file: String,
    pub content_type: String, // "mythicplus"
    pub zone: String,
    pub keystone_level: Option<i64>,
    pub instance_id: Option<i64>,
    pub challenge_id: Option<i64>,
    pub affixes: Vec<i64>,
    pub success: Option<bool>,
    pub duration_ms: Option<i64>,
    /// Raw combat-log timestamps at start/end — the sync anchor.
    pub start_log_timestamp: String,
    pub end_log_timestamp: Option<String>,
    /// Wall-clock ISO-8601 when we started/stopped capture.
    pub started_at: String,
    pub ended_at: Option<String>,
}

struct Active {
    start: ChallengeStart,
    started_at: String,
    video_path: PathBuf,
    stem: String,
}

/// Owns the recorder and the current run, if any. Not internally synchronized;
/// the app wraps it in a mutex.
pub struct Controller {
    recorder: Box<dyn Recorder>,
    config: Config,
    active: Option<Active>,
}

impl Controller {
    pub fn new(recorder: Box<dyn Recorder>, config: Config) -> Self {
        Self { recorder, config, active: None }
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    pub fn handle(&mut self, event: LogEvent) {
        match event {
            LogEvent::ChallengeStart(start) => self.on_start(start),
            LogEvent::ChallengeEnd(end) => self.on_end(end),
        }
    }

    fn on_start(&mut self, start: ChallengeStart) {
        if self.active.is_some() {
            log::warn!("CHALLENGE_MODE_START while already recording; ignoring");
            return;
        }
        let now = Utc::now();
        let level = start.keystone_level.unwrap_or(0);
        let stem = format!(
            "{}_{}_+{}",
            now.format("%Y-%m-%d_%H-%M-%S"),
            sanitize(&start.zone),
            level
        );
        let out_dir = self.config.output_dir();
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            log::error!("cannot create output dir {}: {e}", out_dir.display());
            return;
        }
        let video_path = out_dir.join(format!("{stem}.mp4"));

        match self.recorder.start(&video_path, &self.config) {
            Ok(()) => {
                log::info!(
                    "recording started [{}]: {} +{} -> {}",
                    self.recorder.backend_name(),
                    start.zone,
                    level,
                    video_path.display()
                );
                self.active = Some(Active {
                    start,
                    started_at: now.to_rfc3339(),
                    video_path,
                    stem,
                });
            }
            Err(e) => log::error!("failed to start recording: {e}"),
        }
    }

    fn on_end(&mut self, end: ChallengeEnd) {
        let Some(active) = self.active.take() else {
            log::warn!("CHALLENGE_MODE_END with no active recording; ignoring");
            return;
        };
        // Optional tail so the loot/scoreboard is captured.
        let delay = self.config.stop_delay_secs;
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
        if let Err(e) = self.recorder.stop() {
            log::error!("failed to stop recording: {e}");
        }
        let meta = RecordingMetadata {
            video_file: active.video_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            content_type: "mythicplus".into(),
            zone: active.start.zone.clone(),
            keystone_level: active.start.keystone_level.or(end.keystone_level),
            instance_id: active.start.instance_id.or(end.instance_id),
            challenge_id: active.start.challenge_id,
            affixes: active.start.affixes.clone(),
            success: Some(end.success),
            duration_ms: end.duration_ms,
            start_log_timestamp: active.start.log_timestamp.clone(),
            end_log_timestamp: Some(end.log_timestamp.clone()),
            started_at: active.started_at.clone(),
            ended_at: Some(Utc::now().to_rfc3339()),
        };
        let meta_path = self.config.output_dir().join(format!("{}.json", active.stem));
        match serde_json::to_vec_pretty(&meta) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&meta_path, bytes) {
                    log::error!("failed to write metadata {}: {e}", meta_path.display());
                }
            }
            Err(e) => log::error!("failed to serialize metadata: {e}"),
        }
        log::info!(
            "recording finished: {} ({}), {}",
            active.start.zone,
            if end.success { "timed/completed" } else { "depleted" },
            meta_path.display()
        );
    }
}

/// Make a string safe for a Windows filename.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if r#"<>:"/\|?*,"#.contains(c) { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combatlog::{ChallengeEnd, ChallengeStart};
    use crate::recorder::NoopRecorder;

    fn tmp_config() -> Config {
        let dir = std::env::temp_dir().join(format!("wow-recorder-test-{}", std::process::id()));
        Config { output_directory: dir.to_string_lossy().into_owned(), stop_delay_secs: 0, ..Config::default() }
    }

    #[test]
    fn start_then_end_writes_metadata() {
        let cfg = tmp_config();
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        assert!(!ctrl.is_recording());
        ctrl.handle(LogEvent::ChallengeStart(ChallengeStart {
            zone: "Ara-Kara, City of Echoes".into(),
            instance_id: Some(2648),
            challenge_id: Some(542),
            keystone_level: Some(12),
            affixes: vec![10, 9],
            log_timestamp: "9/14 20:01:05.123".into(),
        }));
        assert!(ctrl.is_recording());
        ctrl.handle(LogEvent::ChallengeEnd(ChallengeEnd {
            instance_id: Some(2648),
            success: true,
            keystone_level: Some(12),
            duration_ms: Some(1_800_000),
            log_timestamp: "9/14 20:31:05.123".into(),
        }));
        assert!(!ctrl.is_recording());
        // A metadata sidecar exists and parses.
        let files: Vec<_> = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        let json = files.iter().find(|p| p.extension().map(|e| e == "json").unwrap_or(false)).expect("metadata json");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(json).unwrap()).unwrap();
        assert_eq!(v["zone"], "Ara-Kara, City of Echoes");
        assert_eq!(v["keystoneLevel"], 12);
        assert_eq!(v["success"], true);
        assert_eq!(v["startLogTimestamp"], "9/14 20:01:05.123");
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn end_without_start_is_ignored() {
        let cfg = tmp_config();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::ChallengeEnd(ChallengeEnd {
            instance_id: Some(1),
            success: false,
            keystone_level: Some(2),
            duration_ms: None,
            log_timestamp: "9/14 20:31:05.123".into(),
        }));
        assert!(!ctrl.is_recording());
    }
}
