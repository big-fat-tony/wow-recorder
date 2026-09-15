mod combatlog;
mod config;
mod recorder;
mod session;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, State};

use config::Config;
use session::Controller;

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
            || my_stop.load(Ordering::Relaxed),
        );
        if let Err(e) = result {
            log::error!("combat-log watcher stopped: {e}");
        }
        watching.store(false, Ordering::Relaxed);
    });
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    watching: bool,
    recording: bool,
    log_directory: String,
    output_directory: String,
    backend: String,
}

#[tauri::command]
fn get_status(state: State<AppState>) -> Status {
    let config = state.config.lock().unwrap().clone();
    let recording = state.controller.lock().unwrap().is_recording();
    Status {
        watching: state.watching.load(Ordering::Relaxed),
        recording,
        log_directory: config.log_directory.clone(),
        output_directory: config.output_directory.clone(),
        backend: if cfg!(feature = "obs") { "obs".into() } else { "noop".into() },
    }
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
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &folder, &sep, &quit])?;

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
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

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
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_status,
            get_config,
            set_config,
            open_recordings_folder,
        ])
        .setup(move |app| {
            build_tray(app.handle())?;
            let state = app.state::<AppState>();
            let log_dir = PathBuf::from(&config.log_directory);
            start_watcher(&state, log_dir);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides to tray instead of quitting.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
