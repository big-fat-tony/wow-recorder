mod combatlog;
mod config;
mod mp4chapters;
mod recorder;
mod session;
mod window;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State};

use serde::Serialize;

use config::Config;
use session::Controller;

/// Holds an update found at startup until the user chooses to install it.
pub struct PendingUpdate(pub Mutex<Option<tauri_plugin_updater::Update>>);

#[derive(Clone, Serialize)]
struct UpdateAvailable {
    version: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ShutdownProgress {
    /// What is being waited on, or `null` once there is nothing left.
    stage: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    downloaded: u64,
    total: Option<u64>,
}

pub struct AppState {
    controller: Arc<Mutex<Controller>>,
    config: Mutex<Config>,
    /// Stop flag of the current watcher thread; replaced when the watcher restarts.
    watch_stop: Mutex<Arc<AtomicBool>>,
    watching: Arc<AtomicBool>,
}

/// Spawn the combat-log tail thread for `log_dir`. Any previous watcher is
/// signalled to stop first.
fn start_watcher(state: &AppState, log_dir: PathBuf) {
    // Signal the previous watcher (if any) to stop, then install a fresh flag.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut slot = state.watch_stop.lock().unwrap();
        slot.store(true, Ordering::Relaxed);
        *slot = stop.clone();
    }
    let controller = state.controller.clone();
    let progress_controller = state.controller.clone();
    let watching = state.watching.clone();
    let my_stop = stop;

    if !log_dir.is_dir() {
        log::warn!("log directory does not exist yet: {}", log_dir.display());
    }
    std::thread::spawn(move || {
        watching.store(true, Ordering::Relaxed);
        log::info!("watching combat logs in {}", log_dir.display());
        let result = combatlog::tail(
            &log_dir,
            |event| {
                if let Ok(mut ctrl) = controller.lock() {
                    ctrl.handle(event);
                }
            },
            || {
                if let Ok(mut ctrl) = progress_controller.lock() {
                    ctrl.note_log_activity();
                }
            },
            || my_stop.load(Ordering::Relaxed),
        );
        if let Err(e) = result {
            log::error!("combat-log watcher stopped: {e}");
        }
        watching.store(false, Ordering::Relaxed);
    });
}

/// Refresh the run-clock overlay once a second while recording. Sleeps until
/// the next whole second of run time so the burned-in clock flips on the tick.
fn start_overlay_ticker(controller: Arc<Mutex<Controller>>) {
    std::thread::spawn(move || loop {
        let wait = {
            let Ok(mut ctrl) = controller.lock() else { break };
            ctrl.tick();
            ctrl.ms_to_next_tick()
        };
        std::thread::sleep(std::time::Duration::from_millis(wait.clamp(50, 1000)));
    });
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    watching: bool,
    recording: bool,
    /// The recording in progress was started by hand, not by a key.
    testing: bool,
    log_directory: String,
    output_directory: String,
    backend: String,
    version: String,
}

#[tauri::command]
fn get_status(state: State<AppState>) -> Status {
    let config = state.config.lock().unwrap().clone();
    let (recording, testing, backend) = {
        let ctrl = state.controller.lock().unwrap();
        (ctrl.is_recording(), ctrl.is_test(), ctrl.backend_name().to_string())
    };
    Status {
        watching: state.watching.load(Ordering::Relaxed),
        recording,
        testing,
        log_directory: config.log_directory.clone(),
        output_directory: config.output_directory.clone(),
        backend,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

/// Record until stopped, for looking at what the settings actually do — where
/// the chat cover lands, whether the overlay reads right, what is on each audio
/// track — without waiting for a key. Answers whether it started.
#[tauri::command]
fn start_test_recording(state: State<AppState>) -> bool {
    state.controller.lock().map(|mut c| c.start_test()).unwrap_or(false)
}

/// End the run being recorded right now and keep the footage — for a key given
/// up on while everyone stands in the dungeon, where the game gives nothing to
/// detect and the log-went-quiet backstop is minutes away. Returns whether
/// there was anything to stop; the capture is closed off the UI thread, since
/// finalizing a multi-gigabyte MP4 takes a moment.
#[tauri::command]
fn stop_recording(state: State<AppState>) -> bool {
    let controller = state.controller.clone();
    let recording = controller.lock().map(|c| c.is_recording()).unwrap_or(false);
    if recording {
        std::thread::spawn(move || {
            if let Ok(mut ctrl) = controller.lock() {
                ctrl.abandon("stopped_by_hand");
            }
        });
    }
    recording
}

/// Quit, finishing the recording first if one is running.
///
/// Closing a multi-gigabyte MP4 takes a few seconds, and a window that simply
/// freezes for them is indistinguishable from one that has hung — so the window
/// comes back, says what it is waiting on, and offers the way out below.
#[tauri::command]
fn quit_app(app: AppHandle, state: State<AppState>) {
    let recording = state.controller.lock().map(|c| c.is_recording()).unwrap_or(false);
    if !recording {
        app.exit(0);
        return;
    }
    show_main_window(&app);
    let controller = state.controller.clone();
    let handle = app.clone();
    std::thread::spawn(move || {
        let report = |stage: &str| {
            log::info!("shutting down: {stage}");
            let _ = handle.emit("shutdown-progress", ShutdownProgress { stage: Some(stage.into()) });
        };
        report("Stopping the capture");
        if let Ok(mut ctrl) = controller.lock() {
            ctrl.abandon_reporting("app_quit", &report);
        }
        let _ = handle.emit("shutdown-progress", ShutdownProgress { stage: None });
        handle.exit(0);
    });
}

/// Quit now and damn the consequences, for when the machine is struggling and
/// the resources matter more than the recording. The capture process is killed
/// rather than orphaned — leaving it running would defeat the whole point —
/// which leaves the video file unfinished.
#[tauri::command]
fn force_quit(app: AppHandle) {
    log::warn!("force quit: killing the capture process, the current recording will be unfinished");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = std::process::Command::new("taskkill")
            .args(["/IM", "extprocess_recorder.exe", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    // Not app.exit(): that runs the exit handler, which waits for the very
    // recording this is meant to walk away from.
    let _ = app;
    std::process::exit(1);
}

#[tauri::command]
fn get_autostart(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled { manager.enable() } else { manager.disable() }.map_err(|e| e.to_string())
}

#[tauri::command]
fn get_config(state: State<AppState>) -> Config {
    state.config.lock().unwrap().clone()
}

#[tauri::command]
fn set_config(state: State<AppState>, config: Config) -> Result<(), String> {
    config.save().map_err(|e| e.to_string())?;
    let log_dir_changed = state.config.lock().unwrap().log_directory != config.log_directory;
    *state.config.lock().unwrap() = config.clone();
    state.controller.lock().unwrap().set_config(config.clone());
    if log_dir_changed {
        start_watcher(&state, PathBuf::from(&config.log_directory));
    }
    Ok(())
}

#[tauri::command]
async fn pick_directory(start: Option<String>) -> Option<String> {
    let mut dialog = rfd::AsyncFileDialog::new().set_title("Choose a folder");
    if let Some(dir) = start.filter(|d| std::path::Path::new(d).is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder().await.map(|f| f.path().to_string_lossy().into_owned())
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WowResolution { width: u32, height: u32 }

/// The WoW window's current client size, if the game is running.
#[tauri::command]
fn get_wow_resolution() -> Option<WowResolution> {
    window::detect_wow_resolution().map(|(width, height)| WowResolution { width, height })
}

#[tauri::command]
fn detect_log_directory() -> Option<String> {
    combatlog::detect_log_directory().map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command]
fn open_recordings_folder(state: State<AppState>) -> Result<(), String> {
    let dir = state.config.lock().unwrap().output_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    open::that(dir).map_err(|e| e.to_string())
}

fn show_main_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "Open recordings folder", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "Stop recording", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &folder, &stop, &sep, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(app.default_window_icon().unwrap().clone())
        .tooltip("WoW Recorder")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "folder" => {
                let state = app.state::<AppState>();
                let _ = open_recordings_folder(state);
            }
            "stop" => {
                if !stop_recording(app.state::<AppState>()) {
                    log::info!("stop requested from the tray, but nothing is recording");
                }
            }
            "quit" => quit_app(app.clone(), app.state::<AppState>()),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Download and install the update found at startup, then relaunch.
#[tauri::command]
async fn install_update(app: AppHandle, state: State<'_, PendingUpdate>) -> Result<(), String> {
    let update = state.0.lock().unwrap().take().ok_or("no pending update")?;
    let mut downloaded: u64 = 0;
    let app_clone = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = app_clone.emit("download-progress", DownloadProgress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    app.restart();
}

#[cfg_attr(debug_assertions, allow(dead_code))]
async fn check_for_update(app: AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    use tauri_plugin_updater::UpdaterExt;
    let Some(update) = app.updater()?.check().await? else {
        return Ok(());
    };
    log::info!("update available: {} -> {}", update.current_version, update.version);
    let version = update.version.clone();
    app.state::<PendingUpdate>().0.lock().unwrap().replace(update);
    app.emit("update-available", UpdateAvailable { version })?;
    Ok(())
}

/// The rolling log file path (next to the config).
pub fn log_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("wow-recorder")
        .join("wow-recorder.log")
}

fn init_logging() {
    use std::io::Write;
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Truncate on start so the file doesn't grow without bound across runs.
    let target: Box<dyn Write + Send> = match std::fs::File::create(&path) {
        Ok(f) => Box::new(f),
        Err(_) => Box::new(std::io::stderr()),
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(target))
        .init();
    log::info!("wow-recorder {} starting", env!("CARGO_PKG_VERSION"));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();

    let config = Config::load();
    let recorder = recorder::make_recorder();
    let controller = Arc::new(Mutex::new(Controller::new(recorder, config.clone())));

    let state = AppState {
        controller,
        config: Mutex::new(config.clone()),
        watch_stop: Mutex::new(Arc::new(AtomicBool::new(false))),
        watching: Arc::new(AtomicBool::new(false)),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, Some(vec![])))
        .manage(state)
        .manage(PendingUpdate(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            set_config,
            open_recordings_folder,
            pick_directory,
            detect_log_directory,
            get_wow_resolution,
            stop_recording,
            start_test_recording,
            quit_app,
            force_quit,
            get_autostart,
            set_autostart,
            install_update,
        ])
        .setup(move |app| {
            build_tray(app.handle())?;
            let state = app.state::<AppState>();
            let log_dir = PathBuf::from(&config.log_directory);
            start_watcher(&state, log_dir);
            start_overlay_ticker(state.controller.clone());
            // Only release builds check for updates; dev builds run as-is.
            #[cfg(not(debug_assertions))]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = check_for_update(handle).await {
                        log::warn!("update check failed: {e}");
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides to tray instead of quitting.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Quitting mid-run would otherwise leave the MP4 unfinalized (and
            // unplayable): stop the capture and keep what was recorded.
            if matches!(event, tauri::RunEvent::Exit) {
                if let Ok(mut ctrl) = app.state::<AppState>().controller.lock() {
                    ctrl.abandon("app_quit");
                }
            }
        });
}
