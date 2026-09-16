//! The recording controller: turns combat-log events into recordings.

use std::path::PathBuf;

use chrono::{Local, NaiveDateTime};
use serde::Serialize;

use crate::combatlog::{parse_log_timestamp, ChallengeEnd, ChallengeStart, LogEvent, UnitDied};
use crate::config::Config;
use crate::mp4chapters::{self, Chapter};
use crate::recorder::Recorder;

/// A point of interest in the recording (currently only player deaths).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Marker {
    pub kind: String, // "death"
    pub title: String,
    /// Milliseconds into the video file.
    pub video_offset_ms: i64,
    /// Milliseconds since `CHALLENGE_MODE_START` (what analyzers show).
    pub run_offset_ms: i64,
    pub log_timestamp: String,
}

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
    /// Wall-clock ISO-8601 when capture actually began (video t=0) / stopped.
    pub started_at: String,
    pub ended_at: Option<String>,
    /// Where `CHALLENGE_MODE_START` (run time 0:00) falls in the video, in ms.
    /// Negative when the run started before capture did (the usual case: OBS
    /// takes a moment to spin up). `video_ms = run_ms + this`.
    pub challenge_start_video_offset_ms: Option<i64>,
    /// Deaths etc., also written into the MP4 as chapters.
    pub markers: Vec<Marker>,
}

struct Active {
    start: ChallengeStart,
    /// Local time of `CHALLENGE_MODE_START` per the log (run clock zero).
    challenge_started: Option<NaiveDateTime>,
    /// Local time the capture began — video t=0.
    capture_started: NaiveDateTime,
    started_at: String,
    video_path: PathBuf,
    stem: String,
    markers: Vec<Marker>,
}

impl Active {
    fn ms_since_capture(&self, t: NaiveDateTime) -> i64 {
        (t - self.capture_started).num_milliseconds()
    }

    fn ms_since_challenge(&self, t: NaiveDateTime) -> Option<i64> {
        self.challenge_started.map(|c| (t - c).num_milliseconds())
    }
}

/// `m:ss` run clock, the format bdk-analyzer / Warcraft Logs use.
pub fn format_run_clock(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
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

    pub fn backend_name(&self) -> &'static str {
        self.recorder.backend_name()
    }

    pub fn handle(&mut self, event: LogEvent) {
        match event {
            LogEvent::ChallengeStart(start) => self.on_start(start),
            LogEvent::ChallengeEnd(end) => self.on_end(end),
            LogEvent::UnitDied(died) => self.on_death(died),
        }
    }

    /// Push the current run clock to the overlay. Called about once a second
    /// from a timer thread while recording; a no-op otherwise.
    pub fn tick(&mut self) {
        if !self.config.timer_overlay {
            return;
        }
        let Some(active) = &self.active else { return };
        let Some(ms) = active.ms_since_challenge(Local::now().naive_local()) else { return };
        if let Err(e) = self.recorder.set_overlay_text(&format_run_clock(ms)) {
            log::debug!("overlay update failed: {e}");
        }
    }

    /// Milliseconds until the run clock next flips to a whole second, so the
    /// overlay can be refreshed right on the tick.
    pub fn ms_to_next_tick(&self) -> u64 {
        let ms = self
            .active
            .as_ref()
            .and_then(|a| a.ms_since_challenge(Local::now().naive_local()))
            .unwrap_or(0)
            .rem_euclid(1000);
        (1000 - ms) as u64
    }

    fn on_start(&mut self, start: ChallengeStart) {
        if self.active.is_some() {
            log::warn!("CHALLENGE_MODE_START while already recording; ignoring");
            return;
        }
        let now = Local::now();
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

        let overlay = self.config.timer_overlay.then(|| format_run_clock(0));
        match self.recorder.start(&video_path, &self.config, overlay.as_deref()) {
            Ok(()) => {
                // Capture is live now (OBS took a moment to start); this is video t=0.
                let capture_started = Local::now().naive_local();
                let challenge_started = parse_log_timestamp(&start.log_timestamp, capture_started);
                if challenge_started.is_none() {
                    log::warn!("unparseable start timestamp {:?}; markers will be unsynced", start.log_timestamp);
                }
                log::info!(
                    "recording started [{}]: {} +{} -> {} (run clock already at {})",
                    self.recorder.backend_name(),
                    start.zone,
                    level,
                    video_path.display(),
                    format_run_clock(
                        challenge_started.map(|c| (capture_started - c).num_milliseconds()).unwrap_or(0)
                    )
                );
                self.active = Some(Active {
                    start,
                    challenge_started,
                    capture_started,
                    started_at: Local::now().to_rfc3339(),
                    video_path,
                    stem,
                    markers: Vec::new(),
                });
            }
            Err(e) => log::error!("failed to start recording: {e}"),
        }
    }

    fn on_death(&mut self, died: UnitDied) {
        let Some(active) = self.active.as_mut() else { return };
        let Some(t) = parse_log_timestamp(&died.log_timestamp, Local::now().naive_local()) else {
            log::warn!("unparseable death timestamp {:?}", died.log_timestamp);
            return;
        };
        // Characters are logged as `Name-Realm`; the realm is noise in a chapter list.
        let name = died.name.split('-').next().unwrap_or(&died.name).to_string();
        let marker = Marker {
            kind: "death".into(),
            title: format!("Death: {name}"),
            video_offset_ms: active.ms_since_capture(t),
            run_offset_ms: active.ms_since_challenge(t).unwrap_or(0),
            log_timestamp: died.log_timestamp,
        };
        log::info!(
            "{} at {} (video {})",
            marker.title,
            format_run_clock(marker.run_offset_ms),
            format_run_clock(marker.video_offset_ms)
        );
        active.markers.push(marker);
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
        write_chapters(&active.video_path, &active.markers);
        let challenge_start_video_offset_ms =
            active.challenge_started.map(|c| (c - active.capture_started).num_milliseconds());
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
            ended_at: Some(Local::now().to_rfc3339()),
            challenge_start_video_offset_ms,
            markers: active.markers.clone(),
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

/// Add the markers to the finished MP4 as chapters (best effort).
fn write_chapters(video: &std::path::Path, markers: &[Marker]) {
    if markers.is_empty() || !video.is_file() {
        return;
    }
    let chapters: Vec<Chapter> = markers
        .iter()
        .map(|m| Chapter { start_ms: m.video_offset_ms.max(0) as u64, title: m.title.clone() })
        .collect();
    match mp4chapters::write_chapters(video, &chapters) {
        Ok(()) => log::info!("wrote {} chapter(s) to {}", chapters.len(), video.display()),
        Err(e) => log::error!("failed to write chapters to {}: {e}", video.display()),
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
    use chrono::Datelike;

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
        // Deaths during the run become markers timed against the run clock.
        let start_ts = chrono::NaiveDate::from_ymd_opt(Local::now().year(), 9, 14)
            .unwrap()
            .and_hms_milli_opt(20, 1, 5, 123)
            .unwrap();
        ctrl.handle(LogEvent::UnitDied(UnitDied {
            name: "Tonyfat-Ravencrest".into(),
            guid: "Player-1096-0A1B2C3D".into(),
            log_timestamp: "9/14 20:15:24.623".into(),
        }));
        {
            let active = ctrl.active.as_ref().unwrap();
            assert_eq!(active.markers.len(), 1);
            assert_eq!(active.markers[0].title, "Death: Tonyfat");
            assert_eq!(active.markers[0].run_offset_ms, 859_500);
            let expected_video =
                (start_ts + chrono::Duration::milliseconds(859_500) - active.capture_started).num_milliseconds();
            assert_eq!(active.markers[0].video_offset_ms, expected_video);
        }
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
        assert_eq!(v["markers"][0]["kind"], "death");
        assert_eq!(v["markers"][0]["runOffsetMs"], 859_500);
        assert!(v["challengeStartVideoOffsetMs"].is_i64());
        std::fs::remove_dir_all(&out).ok();
    }

    #[test]
    fn run_clock_formats_like_the_analyzer() {
        assert_eq!(format_run_clock(0), "0:00");
        assert_eq!(format_run_clock(859_500), "14:19");
        assert_eq!(format_run_clock(-2_500), "0:00");
        assert_eq!(format_run_clock(3_605_000), "60:05");
    }

    #[test]
    fn death_outside_a_run_is_ignored() {
        let cfg = tmp_config();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::UnitDied(UnitDied {
            name: "X-Y".into(),
            guid: "Player-1-2".into(),
            log_timestamp: "9/14 20:15:24.623".into(),
        }));
        assert!(ctrl.active.is_none());
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
