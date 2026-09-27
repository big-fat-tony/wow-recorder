//! The recording controller: turns combat-log events into recordings.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{Local, NaiveDateTime};
use serde::Serialize;

use crate::combatlog::{parse_log_timestamp, ChallengeEnd, ChallengeStart, LogEvent, UnitDied, ZoneChange};
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
    /// `Some` only when WoW logged a `CHALLENGE_MODE_END`; `None` for a key
    /// that was abandoned, since the game never records an outcome for one.
    pub success: Option<bool>,
    pub duration_ms: Option<i64>,
    /// What stopped the capture: `challenge_mode_end`, or why it was abandoned
    /// (`left_dungeon`, `game_closed`, `log_idle`, `app_quit`).
    pub ended_by: String,
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
    /// Started by hand to check the settings, not by a key. It gets no metadata
    /// sidecar: it is not a run, and nothing downstream should treat it as one.
    test: bool,
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
    /// Since when the WoW window has been missing, for the grace period before
    /// a closed game counts as an abandoned key.
    wow_gone_since: Option<Instant>,
    /// When the combat-log watcher last read anything WoW had written. A log
    /// nobody is writing to means a key nobody is playing.
    log_activity: Option<Instant>,
}

/// How long the WoW window has to stay missing before the run counts as
/// abandoned — long enough to ride out a crash-to-desktop-and-relaunch race.
const WOW_GONE_GRACE: Duration = Duration::from_secs(15);


impl Controller {
    pub fn new(recorder: Box<dyn Recorder>, config: Config) -> Self {
        Self { recorder, config, active: None, wow_gone_since: None, log_activity: None }
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    /// Is what is being recorded a test rather than a key?
    pub fn is_test(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.test)
    }

    pub fn backend_name(&self) -> &'static str {
        self.recorder.backend_name()
    }

    pub fn handle(&mut self, event: LogEvent) {
        match event {
            LogEvent::ChallengeStart(start) => self.on_start(start),
            LogEvent::ChallengeEnd(end) => self.on_end(end),
            LogEvent::ZoneChange(zone) => self.on_zone_change(zone),
            LogEvent::LoggingRestarted => {
                // Only meaningful mid-run: the key being recorded was left.
                self.abandon("logging_restarted");
            }
            LogEvent::UnitDied(died) => self.on_death(died),
        }
    }

    /// Called about once a second from a timer thread. Watches for the ways a
    /// key ends without a `CHALLENGE_MODE_END` and pushes the run clock to the
    /// overlay; a no-op when nothing is being recorded.
    pub fn tick(&mut self) {
        if self.active.is_none() {
            self.wow_gone_since = None;
            self.log_activity = None;
            return;
        }
        if let Some(reason) = self.abandon_reason() {
            self.abandon(reason);
            return;
        }
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

    /// Record until told to stop, so the overlay, the chat cover and the audio
    /// tracks can be looked at without waiting for a key. Answers whether it
    /// started.
    pub fn start_test(&mut self) -> bool {
        if self.active.is_some() {
            log::warn!("test recording asked for while already recording; ignoring");
            return false;
        }
        self.begin(
            ChallengeStart {
                zone: "Test".into(),
                instance_id: None,
                challenge_id: None,
                keystone_level: None,
                affixes: Vec::new(),
                log_timestamp: String::new(),
            },
            true,
        );
        self.active.is_some()
    }

    fn on_start(&mut self, start: ChallengeStart) {
        if self.active.is_some() {
            log::warn!("CHALLENGE_MODE_START while already recording; ignoring");
            return;
        }
        self.begin(start, false);
    }

    fn begin(&mut self, start: ChallengeStart, test: bool) {
        let now = Local::now();
        let level = start.keystone_level.unwrap_or(0);
        let stem = if test {
            format!("{}_Test", now.format("%Y-%m-%d_%H-%M-%S"))
        } else {
            format!(
                "{}_{}_+{}",
                now.format("%Y-%m-%d_%H-%M-%S"),
                sanitize(&start.zone),
                level
            )
        };
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
                // A test has no key behind it, so its clock starts with the capture.
                let challenge_started =
                    if test { Some(capture_started) } else { parse_log_timestamp(&start.log_timestamp, capture_started) };
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
                self.log_activity = Some(Instant::now());
                self.active = Some(Active {
                    start,
                    test,
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

    /// Why the active run should be considered abandoned, if it should.
    ///
    /// A key that is given up on leaves no trace in the combat log — no end
    /// event, and often no further lines at all — so the give-up has to be
    /// inferred: the game is gone, or the log stopped growing.
    fn abandon_reason(&mut self) -> Option<&'static str> {
        if crate::window::wow_is_running() {
            self.wow_gone_since = None;
        } else {
            let since = *self.wow_gone_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= WOW_GONE_GRACE {
                return Some("game_closed");
            }
        }

        let timeout = self.config.abandon_timeout();
        if timeout.is_zero() {
            return None;
        }
        let quiet = self.log_activity?.elapsed();
        if quiet < timeout {
            return None;
        }
        log::info!("combat log has not been written to for {}s", quiet.as_secs());
        Some("log_idle")
    }

    /// The watcher read something WoW had written: the log is alive, and so is
    /// whatever is being recorded.
    pub fn note_log_activity(&mut self) {
        self.log_activity = Some(Instant::now());
    }

    fn on_zone_change(&mut self, zone: ZoneChange) {
        let Some(active) = &self.active else { return };
        // Sub-areas inside a dungeon share its instance, so compare on that
        // when the log gave us both ids and fall back to the zone name.
        let left = match (active.start.instance_id, zone.instance_id) {
            (Some(run), Some(now)) => run != now,
            _ => !zone.zone.eq_ignore_ascii_case(&active.start.zone),
        };
        if left {
            log::info!("left {} for {} mid-run", active.start.zone, zone.zone);
            self.abandon("left_dungeon");
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
        if self.active.is_none() {
            log::warn!("CHALLENGE_MODE_END with no active recording; ignoring");
            return;
        }
        // A zeroed end is not an outcome: WoW writes one for the key a new key
        // replaces, so it marks a run that was given up on. There is no
        // scoreboard coming, and the next run is about to start — do not sit
        // through the tail delay holding the capture open.
        if end.is_discarded_key() {
            self.abandon("key_discarded");
            return;
        }
        // Optional tail so the loot/scoreboard is captured.
        let delay = self.config.stop_delay_secs;
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_secs(delay));
        }
        self.finish(Some(end), "challenge_mode_end");
    }

    /// Stop and keep a run the game never ended: the key was given up on, or
    /// WoW (or the app) went away. The footage is worth the same either way —
    /// only the outcome is unknown.
    pub fn abandon(&mut self, reason: &'static str) {
        self.abandon_reporting(reason, &|_| {});
    }

    /// Abandon, telling `progress` what is being waited on. Quitting the app
    /// blocks on the file being closed, and a person watching a frozen window
    /// deserves to know which part of it is taking the time.
    pub fn abandon_reporting(&mut self, reason: &'static str, progress: &dyn Fn(&str)) {
        if self.active.is_some() {
            log::info!("run abandoned ({reason}); keeping the recording");
            self.finish_reporting(None, reason, progress);
        }
    }

    /// Finalize the active recording: stop the capture, write the chapters into
    /// the MP4 and the metadata sidecar next to it.
    fn finish(&mut self, end: Option<ChallengeEnd>, ended_by: &str) {
        self.finish_reporting(end, ended_by, &|_| {});
    }

    fn finish_reporting(&mut self, end: Option<ChallengeEnd>, ended_by: &str, progress: &dyn Fn(&str)) {
        let Some(active) = self.active.take() else { return };
        progress("Stopping the capture");
        self.wow_gone_since = None;
        self.log_activity = None;
        // The capture hands back the game's hook here; the muxer closing the
        // file runs on its own from now on, so the next key can start recording
        // without waiting for it.
        let finalizing = match self.recorder.stop() {
            Ok(finalizing) => Some(finalizing),
            Err(e) => {
                log::error!("failed to stop recording: {e}");
                None
            }
        };
        let challenge_start_video_offset_ms =
            active.challenge_started.map(|c| (c - active.capture_started).num_milliseconds());
        let meta = RecordingMetadata {
            video_file: active.video_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            content_type: "mythicplus".into(),
            zone: active.start.zone.clone(),
            keystone_level: active.start.keystone_level.or(end.as_ref().and_then(|e| e.keystone_level)),
            instance_id: active.start.instance_id.or(end.as_ref().and_then(|e| e.instance_id)),
            challenge_id: active.start.challenge_id,
            affixes: active.start.affixes.clone(),
            success: end.as_ref().map(|e| e.success),
            duration_ms: end.as_ref().and_then(|e| e.duration_ms),
            ended_by: ended_by.to_string(),
            start_log_timestamp: active.start.log_timestamp.clone(),
            end_log_timestamp: end.as_ref().map(|e| e.log_timestamp.clone()),
            started_at: active.started_at.clone(),
            ended_at: Some(Local::now().to_rfc3339()),
            challenge_start_video_offset_ms,
            markers: active.markers.clone(),
        };
        if active.test {
            progress("Closing the video file");
            let video = active.video_path.clone();
            if let Some(finalizing) = finalizing {
                finalizing.wait();
            }
            log::info!("test recording finished: {}", video.display());
            return;
        }

        progress("Saving the run details");
        let meta_path = self.config.output_dir().join(format!("{}.json", active.stem));
        match serde_json::to_vec_pretty(&meta) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&meta_path, bytes) {
                    log::error!("failed to write metadata {}: {e}", meta_path.display());
                }
            }
            Err(e) => log::error!("failed to serialize metadata: {e}"),
        }
        let outcome = match end.as_ref().map(|e| e.success) {
            Some(true) => "timed/completed",
            Some(false) => "depleted",
            None => "abandoned",
        };
        log::info!("recording finished: {} ({outcome}), {}", active.start.zone, meta_path.display());

        // Chapters go into the MP4 itself, so they have to wait for the muxer;
        // quitting cannot, or the app would exit out from under the file.
        let video = active.video_path.clone();
        let markers = active.markers.clone();
        let add_chapters = move |progress: &dyn Fn(&str)| {
            if let Some(finalizing) = finalizing {
                progress("Closing the video file");
                finalizing.wait();
            }
            if !markers.is_empty() {
                progress("Writing chapters into the video");
            }
            write_chapters(&video, &markers);
        };
        if ended_by == "app_quit" {
            add_chapters(progress);
        } else {
            std::thread::spawn(move || add_chapters(&|_| {}));
        }
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
        tmp_config_in("run")
    }

    fn tmp_config_in(name: &str) -> Config {
        let dir = std::env::temp_dir().join(format!("wow-recorder-test-{}-{name}", std::process::id()));
        Config { output_directory: dir.to_string_lossy().into_owned(), stop_delay_secs: 0, ..Config::default() }
    }

    fn start_of(zone: &str, instance: i64) -> ChallengeStart {
        ChallengeStart {
            zone: zone.into(),
            instance_id: Some(instance),
            challenge_id: Some(399),
            keystone_level: Some(17),
            affixes: vec![9, 10],
            log_timestamp: "9/25 16:13:48.815".into(),
        }
    }

    /// Leaving the dungeon abandons the key, and WoW logs no end event for it —
    /// the footage still has to be kept.
    #[test]
    fn leaving_the_dungeon_keeps_the_recording() {
        let cfg = tmp_config_in("abandon");
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::ChallengeStart(start_of("Ruby Life Pools", 2521)));
        ctrl.handle(LogEvent::UnitDied(UnitDied {
            name: "Zarkar-Kazzak".into(),
            guid: "Player-1305-0D73D2F5".into(),
            log_timestamp: "9/25 16:15:37.000".into(),
        }));
        ctrl.handle(LogEvent::ZoneChange(ZoneChange {
            instance_id: Some(2022),
            zone: "The Waking Shores".into(),
            log_timestamp: "9/25 16:17:30.000".into(),
        }));
        assert!(!ctrl.is_recording());

        let files: Vec<_> = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        let json = files.iter().find(|p| p.extension().map(|e| e == "json").unwrap_or(false)).expect("metadata json");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(json).unwrap()).unwrap();
        assert_eq!(v["zone"], "Ruby Life Pools");
        assert_eq!(v["endedBy"], "left_dungeon");
        assert!(v["success"].is_null(), "an abandoned key has no outcome");
        assert!(v["endLogTimestamp"].is_null());
        assert_eq!(v["markers"][0]["title"], "Death: Zarkar");
        std::fs::remove_dir_all(&out).ok();
    }

    /// A key being played is a log being written to, and the watcher reading
    /// it is the only honest evidence of that. An earlier version asked the
    /// filesystem for the log's size instead and cut a timed +17 three minutes
    /// in: Windows leaves that number stale while WoW holds the file open, and
    /// unlike the watcher — which reads to the true end of the file — the check
    /// had nothing to correct it with.
    #[test]
    fn a_log_being_written_to_never_counts_as_idle() {
        let cfg = tmp_config_in("idle");
        let timeout = cfg.abandon_timeout();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::ChallengeStart(start_of("Ruby Life Pools", 2521)));
        assert!(ctrl.abandon_reason().is_none(), "a run that just started is not idle");

        // Half an hour of play, each read arriving just under the timeout.
        for minute in 0..10 {
            ctrl.log_activity = Instant::now().checked_sub(timeout - Duration::from_secs(1));
            assert!(ctrl.abandon_reason().is_none(), "cut at minute {minute}");
            ctrl.note_log_activity();
            assert!(ctrl.abandon_reason().is_none());
        }

        // Nothing read for longer than the timeout: nobody is playing it.
        ctrl.log_activity = Instant::now().checked_sub(timeout + Duration::from_secs(1));
        assert_eq!(ctrl.abandon_reason(), Some("log_idle"));
        std::fs::remove_dir_all(ctrl.config.output_dir()).ok();
    }

    /// A test recording is for looking at, not for keeping records about: it
    /// must not leave a sidecar that tools would read as a run.
    #[test]
    fn a_test_recording_leaves_no_metadata_behind() {
        let cfg = tmp_config_in("testrec");
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);

        assert!(ctrl.start_test(), "should have started");
        assert!(ctrl.is_recording() && ctrl.is_test());
        assert!(!ctrl.start_test(), "a second one must not start on top of the first");
        let stem = ctrl.active.as_ref().unwrap().stem.clone();
        assert!(stem.ends_with("_Test"), "named as a test, got {stem}");
        // Its clock runs from zero, so the overlay can be read.
        assert!(ctrl.active.as_ref().unwrap().challenge_started.is_some());

        ctrl.abandon("stopped_by_hand");
        assert!(!ctrl.is_recording() && !ctrl.is_test());
        let leftovers: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .collect();
        assert!(leftovers.is_empty(), "a test wrote metadata: {leftovers:?}");
        std::fs::remove_dir_all(&out).ok();
    }

    /// A key must still be recorded properly after a test has been taken.
    #[test]
    fn a_key_after_a_test_is_a_normal_run() {
        let cfg = tmp_config_in("testthenrun");
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        assert!(ctrl.start_test());
        ctrl.abandon("stopped_by_hand");

        ctrl.handle(LogEvent::ChallengeStart(start_of("Ruby Life Pools", 2521)));
        assert!(ctrl.is_recording() && !ctrl.is_test());
        ctrl.handle(LogEvent::LoggingRestarted);
        let json: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .collect();
        assert_eq!(json.len(), 1, "exactly the run should have metadata: {json:?}");
        std::fs::remove_dir_all(&out).ok();
    }

    /// Leaving and re-entering writes a fresh logging header, and the zone
    /// change that follows names the same instance as the run being recorded —
    /// the new instance of a dungeon is indistinguishable from a sub-area of
    /// the old one, so the header is the only thing that gives the exit away.
    #[test]
    fn re_entering_the_dungeon_ends_the_run() {
        let cfg = tmp_config_in("relog");
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::ChallengeStart(start_of("Ruby Life Pools", 2521)));
        ctrl.handle(LogEvent::LoggingRestarted);
        assert!(!ctrl.is_recording());

        let files: Vec<_> = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        let json = files.iter().find(|p| p.extension().map(|e| e == "json").unwrap_or(false)).expect("metadata json");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(json).unwrap()).unwrap();
        assert_eq!(v["endedBy"], "logging_restarted");
        assert!(v["success"].is_null());
        std::fs::remove_dir_all(&out).ok();
    }

    /// The zeroed end that a new key writes for the key it replaces is a
    /// discard, not a result: no outcome, and no scoreboard worth waiting for.
    #[test]
    fn a_zeroed_end_is_an_abandoned_key_not_a_depleted_one() {
        let cfg = tmp_config_in("discard");
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::ChallengeStart(start_of("Ruby Life Pools", 2521)));
        ctrl.handle(LogEvent::ChallengeEnd(ChallengeEnd {
            instance_id: Some(2521),
            success: false,
            keystone_level: Some(0),
            duration_ms: Some(0),
            log_timestamp: "9/26 23:49:35.596".into(),
        }));
        assert!(!ctrl.is_recording());

        let files: Vec<_> = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        let json = files.iter().find(|p| p.extension().map(|e| e == "json").unwrap_or(false)).expect("metadata json");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(json).unwrap()).unwrap();
        assert_eq!(v["endedBy"], "key_discarded");
        assert!(v["success"].is_null(), "a discarded key was never depleted");
        std::fs::remove_dir_all(&out).ok();
    }

    /// A real depleted key keeps its result.
    #[test]
    fn a_depleted_key_keeps_its_outcome() {
        let cfg = tmp_config_in("depleted");
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::ChallengeStart(start_of("Ruby Life Pools", 2521)));
        ctrl.handle(LogEvent::ChallengeEnd(ChallengeEnd {
            instance_id: Some(2521),
            success: false,
            keystone_level: Some(17),
            duration_ms: Some(2_100_000),
            log_timestamp: "9/26 23:49:35.596".into(),
        }));
        let files: Vec<_> = std::fs::read_dir(&out).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        let json = files.iter().find(|p| p.extension().map(|e| e == "json").unwrap_or(false)).expect("metadata json");
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(json).unwrap()).unwrap();
        assert_eq!(v["endedBy"], "challenge_mode_end");
        assert_eq!(v["success"], false);
        std::fs::remove_dir_all(&out).ok();
    }

    /// Sub-areas inside the dungeon share its instance id and must not stop it.
    #[test]
    fn zone_change_inside_the_dungeon_keeps_recording() {
        let cfg = tmp_config_in("subzone");
        let out = cfg.output_dir();
        let mut ctrl = Controller::new(Box::new(NoopRecorder::default()), cfg);
        ctrl.handle(LogEvent::ChallengeStart(start_of("Ruby Life Pools", 2521)));
        ctrl.handle(LogEvent::ZoneChange(ZoneChange {
            instance_id: Some(2521),
            zone: "Ruby Life Pools".into(),
            log_timestamp: "9/25 16:14:01.551".into(),
        }));
        assert!(ctrl.is_recording());
        std::fs::remove_dir_all(&out).ok();
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
