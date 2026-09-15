//! Video capture abstraction.
//!
//! The core app drives a [`Recorder`] trait so the combat-log state machine can
//! be built and tested without the OBS toolchain. The real capture backend
//! (libobs-recorder) is added behind the `obs` feature; until then [`NoopRecorder`]
//! stands in and records nothing but the timing.

use std::path::Path;

use crate::config::Config;

/// The WoW window, for the capture backend to target (used by the `obs` backend).
#[allow(dead_code)]
pub const WOW_WINDOW_TITLE: &str = "World of Warcraft";
#[allow(dead_code)]
pub const WOW_WINDOW_CLASS: &str = "GxWindowClass";
#[allow(dead_code)]
pub const WOW_WINDOW_PROCESS: &str = "Wow.exe";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[allow(dead_code)]
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

/// Records nothing; used until the OBS backend is wired, and for tests.
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
        log::warn!(
            "NoopRecorder: pretending to record to {} (build with --features obs for real capture)",
            output.display()
        );
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if !self.running {
            return Err(Error::Idle);
        }
        self.running = false;
        log::warn!("NoopRecorder: stopped");
        Ok(())
    }

    fn backend_name(&self) -> &'static str {
        "noop"
    }
}

/// Construct the configured capture backend.
pub fn make_recorder() -> Box<dyn Recorder> {
    #[cfg(feature = "obs")]
    {
        match obs::ObsRecorder::new() {
            Ok(r) => return Box::new(r),
            Err(e) => log::error!("failed to init OBS recorder, falling back to noop: {e}"),
        }
    }
    Box::new(NoopRecorder::default())
}

#[cfg(feature = "obs")]
mod obs {
    // Real libobs-recorder backend — added once the OBS build is in place.
    // Kept behind the `obs` feature so the core crate builds without it.
    use super::*;

    pub struct ObsRecorder {
        // recorder: libobs_recorder::Recorder,
    }

    impl ObsRecorder {
        pub fn new() -> Result<Self> {
            Err(Error::Backend("OBS backend not yet implemented".into()))
        }
    }

    impl Recorder for ObsRecorder {
        fn start(&mut self, _output: &Path, _config: &Config) -> Result<()> {
            Err(Error::Backend("OBS backend not yet implemented".into()))
        }
        fn stop(&mut self) -> Result<()> {
            Err(Error::Backend("OBS backend not yet implemented".into()))
        }
        fn backend_name(&self) -> &'static str {
            "obs"
        }
    }
}
