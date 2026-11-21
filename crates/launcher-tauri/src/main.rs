#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

use commands::AppState;
use launcher_core::{LauncherCore, LauncherPaths};
use launcher_db::{FileSecretStore, LauncherDb};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::broadcast::error::RecvError;

fn resolve_data_root() -> PathBuf {
    if let Ok(custom) = std::env::var("OPENSYNC_LAUNCHER_DATA")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.trim().is_empty()
    {
        return PathBuf::from(xdg).join("opensync-launcher");
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home).join(".local/share/opensync-launcher");
    }
    PathBuf::from("./opensync-launcher-data")
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let data_root = resolve_data_root();
    if let Err(err) = std::fs::create_dir_all(&data_root) {
        tracing::error!(path = %data_root.display(), error = %err, "failed to create launcher data directory");
    }

    let db = LauncherDb::open(data_root.join("launcher.db"))
        .expect("failed to initialize launcher SQLite database");
    let secrets = FileSecretStore::new(data_root.join("secrets.json"));
    let paths = LauncherPaths::under(&data_root);
    let core = Arc::new(LauncherCore::new(db, secrets, paths));
    let app_state = AppState::new(Arc::clone(&core));

    tauri::Builder::default()
        .manage(app_state)
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let state = app.state::<AppState>();
            let core_for_events = Arc::clone(&state.core);
            let core_for_restore = Arc::clone(&state.core);

            let mut rx = core_for_events.subscribe_events();
            tauri::async_runtime::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(evt) => {
                            if let Err(err) = app_handle.emit("launcher-event", &evt) {
                                tracing::warn!(error = %err, "failed to emit launcher-event");
                            }
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            tracing::debug!(skipped, "launcher event receiver lagged");
                        }
                        Err(RecvError::Closed) => break,
                    }
                }
            });

            tauri::async_runtime::spawn(async move {
                if let Err(err) = core_for_restore.restore_remembered_profile().await {
                    tracing::warn!(error = %err, "failed to restore remembered profile");
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_bootstrap_state,
            commands::connect_profile,
            commands::switch_profile,
            commands::delete_profile,
            commands::reconnect_now,
            commands::list_library,
            commands::get_game_cover_data_url,
            commands::start_game_download,
            commands::pause_download,
            commands::resume_download,
            commands::cancel_download,
            commands::set_bandwidth_limit,
            commands::list_downloads,
            commands::launch_game,
            commands::sync_game_now,
            commands::uninstall_game,
            commands::set_game_executable,
            commands::set_game_proton,
            commands::list_proton_installations,
            commands::fetch_latest_ge_proton,
            commands::download_ge_proton,
        ])
        .run(tauri::generate_context!())
        .expect("error while running OpenSync Launcher");
}
