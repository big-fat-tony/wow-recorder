//! Video capture abstraction.
//!
//! The core app drives a [`Recorder`] trait; [`ObsRecorder`] is the real backend
//! (libobs-recorder, capturing the WoW window out-of-process via
//! `extprocess_recorder.exe`), and [`NoopRecorder`] stands in when the OBS
//! runtime is missing (and in tests).

use std::path::{Path, PathBuf};

use libobs_recorder::settings::{
    AudioSource, Corner, Framerate, RateControl, RecorderSettings, Resolution, TextOverlay, Window,
};
use crate::config::FALLBACK_RESOLUTION;
use crate::window::detect_wow_resolution;
use libobs_recorder::Recorder as ObsInner;

use crate::config::Config;

/// The WoW window the capture backend targets.
pub const WOW_WINDOW_TITLE: &str = "World of Warcraft";
/// Retail's window class; older clients used `GxWindowClass`. The capture
/// sources are told to match on the executable, so this is only a tiebreak.
pub const WOW_WINDOW_CLASS: &str = "waApplication Window";
pub const WOW_WINDOW_PROCESS: &str = "Wow.exe";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("capture backend error: {0}")]
    Backend(String),
    #[error("recorder is already running")]
    Busy,
    #[error("no recording in progress")]
    Idle,
}

pub type Result<T> = std::result::Result<T, Error>;

pub trait Recorder: Send {
    /// Begin capturing the WoW window to `output` (an `.mp4` path). With
    /// `overlay_text`, a text overlay showing it is burned into the corner.
    fn start(&mut self, output: &Path, config: &Config, overlay_text: Option<&str>) -> Result<()>;
    /// Replace the overlay text while recording.
    fn set_overlay_text(&mut self, text: &str) -> Result<()>;
    /// Give up the game's graphics hook and stop capturing. Returns a handle to
    /// the slow half — the muxer closing the file, the capture process exiting
    /// — which nothing but reading that file has to wait for.
    fn stop(&mut self) -> Result<Finalizing>;
    /// A short name for logging.
    fn backend_name(&self) -> &'static str;
}

/// Path to the bundled `extprocess_recorder.exe` next to the app executable.
fn extprocess_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("libobs").join("extprocess_recorder.exe");
    path.exists().then_some(path)
}

/// A recording that has let go of the game but is still being written: the
/// muxer closing the file, the capture process exiting. The next recording does
/// not wait for any of it; only reading the finished file does.
pub struct Finalizing {
    join: Option<std::thread::JoinHandle<()>>,
}

impl Finalizing {
    /// Nothing left to do (the noop backend, or a stop that failed outright).
    fn done() -> Self {
        Self { join: None }
    }

    fn spawn(work: impl FnOnce() + Send + 'static) -> Self {
        Self { join: Some(std::thread::spawn(work)) }
    }

    /// Block until the file is closed and safe to read or rewrite.
    pub fn wait(self) {
        if let Some(join) = self.join {
            let _ = join.join();
        }
    }
}

/// libobs-recorder backend. A fresh inner recorder is created per run (as in
/// league_record) and shut down when the run stops.
pub struct ObsRecorder {
    extprocess: PathBuf,
    current: Option<ObsInner>,
}

impl ObsRecorder {
    pub fn new() -> Result<Self> {
        let extprocess = extprocess_path()
            .ok_or_else(|| Error::Backend("extprocess_recorder.exe not found next to the app".into()))?;
        Ok(Self { extprocess, current: None })
    }
}

/// Overlay font size as a fraction of the output height (~28 px at 1080p).
const OVERLAY_FONT_SCALE: f64 = 0.026;

impl Recorder for ObsRecorder {
    fn start(&mut self, output: &Path, config: &Config, overlay_text: Option<&str>) -> Result<()> {
        if self.current.is_some() {
            return Err(Error::Busy);
        }
        let input = detect_wow_resolution().unwrap_or(FALLBACK_RESOLUTION);
        let output_res = config.output_resolution(input);
        log::info!("capture input {input:?} -> output {output_res:?}, cqp {}", config.cqp());
        let mut settings = RecorderSettings::new(
            Window::new(
                WOW_WINDOW_TITLE,
                Some(WOW_WINDOW_CLASS.into()),
                Some(WOW_WINDOW_PROCESS.into()),
            ),
            Resolution::new(input.0, input.1),
            Resolution::new(output_res.0, output_res.1),
            output,
        );
        settings.set_framerate(Framerate::new(config.fps, 1));
        settings.set_rate_control(RateControl::CQP(config.cqp()));
        if config.audio_mode == "tracks" {
            // Two separate streams in the file: the game alone (what most
            // players open by default), then everything the PC plays plus the
            // microphone — comms and commentary, mutable in one click.
            settings.set_audio_tracks(vec![AudioSource::APPLICATION, AudioSource::ALL]);
        } else {
            let audio = match config.audio_mode.as_str() {
                "off" => AudioSource::NONE,
                "desktop_mic" => AudioSource::ALL,   // desktop output + microphone
                "game" => AudioSource::APPLICATION,  // only the WoW window's audio
                _ => AudioSource::SYSTEM,             // "desktop": default output, no mic
            };
            settings.set_audio_source(audio);
        }
        settings.set_microphone_gain_db(config.mic_gain());
        settings.set_desktop_gain_db(config.desktop_gain());
        if let Some(text) = overlay_text {
            let font_size = ((output_res.1 as f64 * OVERLAY_FONT_SCALE).round() as u32).max(12);
            settings.set_text_overlay(TextOverlay::new(text, font_size, Corner::TopLeft));
        }

        let mut recorder = ObsInner::new_with_paths(Some(&self.extprocess), None, None, None)
            .map_err(|e| Error::Backend(format!("init: {e:?}")))?;
        recorder.configure(&settings).map_err(|e| Error::Backend(format!("configure: {e:?}")))?;
        if let Ok(adapter) = recorder.adapter_info() {
            log::info!("OBS adapter: {adapter:?}");
        }
        if let Ok(encoder) = recorder.selected_encoder() {
            log::info!("OBS encoder: {encoder:?}");
        }
        recorder.start_recording().map_err(|e| Error::Backend(format!("start: {e:?}")))?;
        self.current = Some(recorder);
        Ok(())
    }

    fn set_overlay_text(&mut self, text: &str) -> Result<()> {
        let recorder = self.current.as_mut().ok_or(Error::Idle)?;
        recorder.set_overlay_text(text).map_err(|e| Error::Backend(format!("overlay: {e:?}")))
    }

    fn stop(&mut self) -> Result<Finalizing> {
        let mut recorder = self.current.take().ok_or(Error::Idle)?;
        // Hand the hook back before anything else. Only one capture client can
        // hold a game's hook, so this is the whole of what the next recording
        // is waiting for — the file it leaves behind is not its problem.
        if let Err(e) = recorder.release_capture() {
            log::warn!("could not release the capture hook: {e:?}");
        }
        Ok(Finalizing::spawn(move || {
            let stopped = recorder.stop_recording();
            let shutdown = recorder.shutdown();
            log::info!("OBS stop: stop={stopped:?} shutdown={shutdown:?}");
        }))
    }

    fn backend_name(&self) -> &'static str {
        "obs"
    }
}

/// Records nothing; used when the OBS runtime is missing, and in tests.
#[derive(Default)]
pub struct NoopRecorder {
    running: bool,
}

impl Recorder for NoopRecorder {
    fn start(&mut self, output: &Path, _config: &Config, _overlay_text: Option<&str>) -> Result<()> {
        if self.running {
            return Err(Error::Busy);
        }
        self.running = true;
        log::warn!("NoopRecorder: no OBS runtime; not capturing {}", output.display());
        Ok(())
    }

    fn set_overlay_text(&mut self, _text: &str) -> Result<()> {
        if !self.running {
            return Err(Error::Idle);
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<Finalizing> {
        if !self.running {
            return Err(Error::Idle);
        }
        self.running = false;
        Ok(Finalizing::done())
    }

    fn backend_name(&self) -> &'static str {
        "noop"
    }
}

/// Construct the capture backend, falling back to noop if OBS is unavailable.
pub fn make_recorder() -> Box<dyn Recorder> {
    match ObsRecorder::new() {
        Ok(r) => Box::new(r),
        Err(e) => {
            log::warn!("OBS backend unavailable ({e}); using noop recorder");
            Box::new(NoopRecorder::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-OBS smoke test: records a few seconds (a black canvas if WoW isn't
    /// running) with the overlay clock ticking, then leaves the file for
    /// inspection. `WOW_RECORDER_SMOKE_AUDIO` picks the audio mode (default
    /// `tracks`), `WOW_RECORDER_SMOKE_SECS` the duration,
    /// `WOW_RECORDER_SMOKE_MIC_DB` the microphone boost and
    /// `WOW_RECORDER_SMOKE_DESKTOP_DB` the desktop pad.
    ///
    /// Needs the OBS runtime next to the test binary: copy `target/debug/libobs`
    /// into the directory of the `.exe` that `cargo test` prints, then run
    /// `cargo test obs_overlay_smoke -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn obs_overlay_smoke() {
        // OBS reports what it captures (and what it fails to find) through the
        // `log` crate, so surface it: `RUST_LOG=info` shows the capture setup.
        let _ = env_logger::builder().is_test(false).try_init();
        let audio_mode = std::env::var("WOW_RECORDER_SMOKE_AUDIO").unwrap_or_else(|_| "tracks".into());
        let secs: u64 = std::env::var("WOW_RECORDER_SMOKE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(3);
        let mic_gain_db: f32 = std::env::var("WOW_RECORDER_SMOKE_MIC_DB").ok().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let desktop_gain_db: f32 = std::env::var("WOW_RECORDER_SMOKE_DESKTOP_DB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(Config::default().desktop_gain_db);
        let out = std::env::temp_dir().join(format!("wow-recorder-smoke-{audio_mode}.mp4"));
        let _ = std::fs::remove_file(&out);
        let cfg = Config { output_mode: "raw".into(), audio_mode, mic_gain_db, desktop_gain_db, ..Config::default() };
        let mut rec = ObsRecorder::new().expect("OBS runtime next to the test exe");
        rec.start(&out, &cfg, Some("0:00")).unwrap();
        for i in 1..=secs {
            std::thread::sleep(std::time::Duration::from_secs(1));
            rec.set_overlay_text(&format!("{}:{:02}", i / 60, i % 60)).unwrap();
        }
        rec.stop().unwrap();
        assert!(out.is_file(), "no output at {}", out.display());
        println!("wrote {}", out.display());
    }
}
