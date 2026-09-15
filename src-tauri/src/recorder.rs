//! Video capture abstraction.
//!
//! The core app drives a [`Recorder`] trait; [`ObsRecorder`] is the real backend
//! (libobs-recorder, capturing the WoW window out-of-process via
//! `extprocess_recorder.exe`), and [`NoopRecorder`] stands in when the OBS
//! runtime is missing (and in tests).

use std::path::{Path, PathBuf};

use libobs_recorder::settings::{AudioSource, Framerate, RateControl, RecorderSettings, Resolution, Window};
use libobs_recorder::Recorder as ObsInner;

use crate::config::Config;

/// The WoW window the capture backend targets.
pub const WOW_WINDOW_TITLE: &str = "World of Warcraft";
pub const WOW_WINDOW_CLASS: &str = "GxWindowClass";
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
    /// Begin capturing the WoW window to `output` (an `.mp4` path).
    fn start(&mut self, output: &Path, config: &Config) -> Result<()>;
    /// Stop the current capture and finalize the file.
    fn stop(&mut self) -> Result<()>;
    /// A short name for logging.
    fn backend_name(&self) -> &'static str;
}

/// Path to the bundled `extprocess_recorder.exe` next to the app executable.
fn extprocess_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.join("libobs").join("extprocess_recorder.exe");
    path.exists().then_some(path)
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

impl Recorder for ObsRecorder {
    fn start(&mut self, output: &Path, config: &Config) -> Result<()> {
        if self.current.is_some() {
            return Err(Error::Busy);
        }
        let resolution: Resolution = Resolution::new(config.width, config.height);
        let mut settings = RecorderSettings::new(
            Window::new(
                WOW_WINDOW_TITLE,
                Some(WOW_WINDOW_CLASS.into()),
                Some(WOW_WINDOW_PROCESS.into()),
            ),
            resolution,
            resolution,
            output,
        );
        settings.set_framerate(Framerate::new(config.fps, 1));
        settings.set_rate_control(RateControl::CBR(config.bitrate_kbps));
        settings.set_audio_source(if config.record_audio { AudioSource::ALL } else { AudioSource::NONE });

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

    fn stop(&mut self) -> Result<()> {
        let recorder = self.current.take().ok_or(Error::Idle)?;
        let mut recorder = recorder;
        let stopped = recorder.stop_recording();
        let shutdown = recorder.shutdown();
        log::info!("OBS stop: stop={stopped:?} shutdown={shutdown:?}");
        stopped.map_err(|e| Error::Backend(format!("stop: {e:?}")))?;
        Ok(())
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
    fn start(&mut self, output: &Path, _config: &Config) -> Result<()> {
        if self.running {
            return Err(Error::Busy);
        }
        self.running = true;
        log::warn!("NoopRecorder: no OBS runtime; not capturing {}", output.display());
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if !self.running {
            return Err(Error::Idle);
        }
        self.running = false;
        Ok(())
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
