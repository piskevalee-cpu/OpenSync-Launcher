use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use launcher_core::{
    ConnectionStateDto, CoreError, DownloadJobSnapshot, InterruptedSessionDto, LauncherCore,
    ReconnectPolicy, ShutdownResultDto, ShutdownStatusDto, UploadJobSnapshot, default_games_dir,
};
use launcher_db::{ActivityLogEntry, InstalledGame, PendingSync, PlaytimeSummary, Profile};
use opensync_api::{ApiError, Game, User};
use opensync_proton::{
    BinaryKind, DiscoveredExecutable, GeProtonRelease, ProtonInstallation, detect_binary_kind,
    discover_executables,
};
use opensync_runner::LaunchOptions;
use opensync_sync::SyncReport;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tauri::State;

pub struct AppState {
    pub core: Arc<LauncherCore>,
    pub bandwidth_limit_bps: AtomicU64,
    pub allow_window_close: AtomicBool,
}

impl AppState {
    pub fn new(core: Arc<LauncherCore>) -> Self {
        Self {
            core,
            bandwidth_limit_bps: AtomicU64::new(0),
            allow_window_close: AtomicBool::new(false),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BootstrapStateDto {
    pub active_profile: Option<Profile>,
    pub profiles: Vec<Profile>,
    pub connection_state: ConnectionStateDto,
    pub proton_installations: Vec<ProtonInstallation>,
    pub pending_syncs: Vec<PendingSync>,
    pub downloads: Vec<DownloadJobSnapshot>,
    pub bandwidth_limit_bps: u64,
    pub games_dir: String,
    pub activity_logs: Vec<ActivityLogEntry>,
    pub interrupted_sessions: Vec<InterruptedSessionDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconnectOutcomeDto {
    pub attempts: u32,
    pub flushed_syncs: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct FolderScanDto {
    pub file_count: usize,
    pub total_bytes: u64,
    pub name: String,
    pub top_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DroppedPathDto {
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub is_image: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalImageDto {
    pub base64: String,
    pub content_type: String,
    pub data_url: String,
    pub name: String,
}

/// Avatar bytes from the webview: either a base64 string (with or without a data-URL
/// prefix) or a JSON byte array. Both are accepted so a file input/drop just works.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum PfpBytesInput {
    Base64(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Serialize)]
pub struct ExecutableOptionDto {
    pub rel_path: String,
    pub kind: BinaryKind,
    pub size: u64,
}

impl From<DiscoveredExecutable> for ExecutableOptionDto {
    fn from(e: DiscoveredExecutable) -> Self {
        Self {
            rel_path: e.rel_path,
            kind: e.kind,
            size: e.size,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LibraryGameDto {
    pub game_id: i64,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub system_requirements: String,
    pub uploader_name: Option<String>,
    pub uploaded_by: Option<i64>,
    pub uploader_pfp: Option<String>,
    pub created_at: String,
    pub status: String,
    pub total_size: u64,
    pub download_count: u64,
    pub has_cover: bool,
    pub has_overlay: bool,
    pub installed: bool,
    pub install_dir: Option<String>,
    pub executable_rel_path: Option<String>,
    pub discovered_executables: Vec<ExecutableOptionDto>,
    pub binary_kind: Option<BinaryKind>,
    pub proton_override: Option<String>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub pending_sync: bool,
    pub playtime: PlaytimeSummary,
}

fn err_str(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_bootstrap_state(state: State<'_, AppState>) -> Result<BootstrapStateDto, String> {
    let active_profile = state.core.active_profile().await;
    let profiles = state.core.list_profiles().map_err(err_str)?;
    let connection_state = ConnectionStateDto::from(state.core.connection_state().await);
    let core_for_proton = Arc::clone(&state.core);
    let proton_installations = tokio::task::spawn_blocking(move || core_for_proton.detect_proton())
        .await
        .map_err(|e| format!("proton scan failed: {e}"))?;
    let pending_syncs = state.core.list_pending_syncs().await.unwrap_or_default();
    let downloads = state.core.list_downloads().await;
    let bandwidth_limit_bps = state.bandwidth_limit_bps.load(Ordering::Relaxed);
    let games_dir = state.core.games_dir().to_string_lossy().to_string();
    let interrupted_sessions = state
        .core
        .detect_interrupted_sessions()
        .await
        .unwrap_or_default();
    let activity_logs = state.core.list_activity_logs(80).await.unwrap_or_default();

    Ok(BootstrapStateDto {
        active_profile,
        profiles,
        connection_state,
        proton_installations,
        pending_syncs,
        downloads,
        bandwidth_limit_bps,
        games_dir,
        activity_logs,
        interrupted_sessions,
    })
}

fn expand_home(raw: &str) -> PathBuf {
    let home = std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|h| !h.trim().is_empty())
        });
    if raw == "~" {
        if let Some(home) = home {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(raw)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_games_dir(state: State<'_, AppState>) -> Result<String, String> {
    let core = Arc::clone(&state.core);
    tokio::task::spawn_blocking(move || Ok(core.games_dir().to_string_lossy().to_string()))
        .await
        .map_err(|e| format!("get_games_dir task failed: {e}"))?
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_games_dir(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let raw = path.trim().to_string();
    if raw.is_empty() {
        return Err("Games directory cannot be empty".to_string());
    }
    let core = Arc::clone(&state.core);
    tokio::task::spawn_blocking(move || {
        let target = if raw == "auto" {
            default_games_dir()
        } else {
            expand_home(&raw)
        };
        let resolved = core.set_games_dir(target).map_err(err_str)?;
        Ok(resolved.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| format!("set_games_dir task failed: {e}"))?
}

#[tauri::command(rename_all = "snake_case")]
pub async fn connect_profile(
    state: State<'_, AppState>,
    label: String,
    server_url: String,
    username: String,
    password: String,
    register: bool,
) -> Result<Profile, String> {
    if register {
        state
            .core
            .register_and_connect(&label, &server_url, &username, &password)
            .await
            .map_err(err_str)
    } else {
        state
            .core
            .connect_with_password(&label, &server_url, &username, &password)
            .await
            .map_err(err_str)
    }
}

#[tauri::command(rename_all = "snake_case")]
pub async fn switch_profile(
    state: State<'_, AppState>,
    profile_id: String,
) -> Result<Profile, String> {
    state
        .core
        .switch_profile(&profile_id)
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn delete_profile(
    state: State<'_, AppState>,
    profile_id: String,
) -> Result<bool, String> {
    state
        .core
        .delete_profile(&profile_id)
        .await
        .map_err(err_str)?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_current_user_profile(state: State<'_, AppState>) -> Result<User, String> {
    state
        .core
        .get_current_user_profile()
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_user_pfp_data_url(
    state: State<'_, AppState>,
    pfp_path: Option<String>,
    user_id: i64,
) -> Result<Option<String>, String> {
    match state
        .core
        .fetch_cached_user_pfp(pfp_path.as_deref(), user_id)
        .await
    {
        Ok(Some((bytes, mime))) => {
            let url = tokio::task::spawn_blocking(move || {
                let encoded = BASE64.encode(&bytes);
                format!("data:{mime};base64,{encoded}")
            })
            .await
            .map_err(|e| format!("encode pfp failed: {e}"))?;
            Ok(Some(url))
        }
        Ok(None) => Ok(None),
        Err(CoreError::Api(ApiError::Http { status: 404, .. })) => Ok(None),
        Err(CoreError::Api(ApiError::Network(_))) => Ok(None),
        Err(e) => Err(err_str(e)),
    }
}

#[tauri::command(rename_all = "snake_case")]
pub async fn upload_profile_pfp(
    state: State<'_, AppState>,
    payload: PfpBytesInput,
    content_type: String,
) -> Result<String, String> {
    let bytes = match payload {
        PfpBytesInput::Bytes(bytes) => bytes,
        PfpBytesInput::Base64(raw) => {
            let trimmed = raw.trim();
            let encoded = trimmed
                .split_once(',')
                .map(|(_, rest)| rest)
                .unwrap_or(trimmed);
            BASE64
                .decode(encoded)
                .map_err(|e| format!("invalid base64 image: {e}"))?
        }
    };
    state
        .core
        .upload_active_profile_pfp(bytes, &content_type)
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn remove_profile_pfp(state: State<'_, AppState>) -> Result<String, String> {
    state
        .core
        .remove_active_profile_pfp()
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn delete_server_account(state: State<'_, AppState>) -> Result<bool, String> {
    state
        .core
        .delete_active_account()
        .await
        .map_err(err_str)?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn rename_profile(
    state: State<'_, AppState>,
    label: String,
) -> Result<Profile, String> {
    state.core.rename_profile(&label).await.map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn reconnect_now(state: State<'_, AppState>) -> Result<ReconnectOutcomeDto, String> {
    let outcome = state
        .core
        .reconnect_with_backoff(ReconnectPolicy::default())
        .await
        .map_err(err_str)?;
    Ok(ReconnectOutcomeDto {
        attempts: outcome.attempts,
        flushed_syncs: outcome.flushed_syncs,
    })
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_library(state: State<'_, AppState>) -> Result<Vec<LibraryGameDto>, String> {
    let Some(profile) = state.core.active_profile().await else {
        return Ok(Vec::new());
    };

    let remote_games: Vec<Game> = state.core.fetch_remote_games().await.unwrap_or_default();
    let installed_games: Vec<InstalledGame> =
        state.core.list_downloaded_games().await.unwrap_or_default();
    let pending_set: HashSet<i64> = state
        .core
        .list_pending_syncs()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.game_id)
        .collect();

    let mut installed_map: HashMap<i64, InstalledGame> = installed_games
        .into_iter()
        .map(|g| (g.game_id, g))
        .collect();

    // Scan installed directories on a blocking thread so the async runtime stays responsive.
    let scan_targets: Vec<(i64, std::path::PathBuf, String, Option<String>)> = installed_map
        .values()
        .map(|inst| {
            (
                inst.game_id,
                inst.install_dir.clone(),
                inst.name.clone(),
                inst.executable_rel_path.clone(),
            )
        })
        .collect();

    let discovered_map: HashMap<i64, (Vec<ExecutableOptionDto>, Option<BinaryKind>)> =
        tokio::task::spawn_blocking(move || {
            let mut out = HashMap::new();
            for (gid, dir, name, exe_override) in scan_targets {
                let discovered = if dir.is_dir() {
                    discover_executables(&dir, &name)
                } else {
                    Vec::new()
                };
                let selected_kind = exe_override
                    .as_deref()
                    .and_then(|rel| {
                        let abs = dir.join(rel);
                        if abs.is_file() {
                            Some(detect_binary_kind(&abs))
                        } else {
                            None
                        }
                    })
                    .or_else(|| discovered.first().map(|e| e.kind));
                let opts = discovered.into_iter().map(ExecutableOptionDto::from).collect();
                out.insert(gid, (opts, selected_kind));
            }
            out
        })
        .await
        .unwrap_or_default();

    let playtime_map = state
        .core
        .db()
        .playtime_summaries_for_profile(&profile.id)
        .unwrap_or_default();

    let mut out = Vec::with_capacity(remote_games.len() + installed_map.len());
    let mut seen_ids = HashSet::new();

    for remote in remote_games {
        seen_ids.insert(remote.id);
        let inst = installed_map.remove(&remote.id);
        let playtime = playtime_map
            .get(&remote.id)
            .cloned()
            .unwrap_or(PlaytimeSummary {
                total_secs: 0,
                session_count: 0,
                last_played_at: None,
            });
        let (discovered_executables, binary_kind) = discovered_map
            .get(&remote.id)
            .cloned()
            .unwrap_or_else(|| (Vec::new(), None));

        out.push(LibraryGameDto {
            game_id: remote.id,
            name: remote.name,
            slug: remote.slug,
            description: remote.description,
            system_requirements: remote.system_requirements,
            uploader_name: remote.uploader_name,
            uploaded_by: remote.uploaded_by,
            uploader_pfp: remote.uploader_pfp,
            created_at: remote.created_at,
            status: remote.status,
            total_size: inst.as_ref().map(|i| i.total_size.max(remote.total_size)).unwrap_or(remote.total_size),
            download_count: remote.download_count,
            has_cover: remote.cover.is_some(),
            has_overlay: remote.has_overlay,
            installed: inst.is_some(),
            install_dir: inst.as_ref().map(|i| i.install_dir.to_string_lossy().to_string()),
            executable_rel_path: inst.as_ref().and_then(|i| i.executable_rel_path.clone()),
            discovered_executables,
            binary_kind,
            proton_override: inst.as_ref().and_then(|i| i.proton_override.clone()),
            last_synced_at: inst.as_ref().and_then(|i| i.last_synced_at),
            pending_sync: pending_set.contains(&remote.id),
            playtime,
        });
    }

    // Include locally installed games even when offline or not present in remote response.
    for (gid, inst) in installed_map {
        if seen_ids.contains(&gid) {
            continue;
        }
        let playtime = playtime_map
            .get(&gid)
            .cloned()
            .unwrap_or(PlaytimeSummary {
                total_secs: 0,
                session_count: 0,
                last_played_at: None,
            });
        let (discovered_executables, binary_kind) = discovered_map
            .get(&gid)
            .cloned()
            .unwrap_or_else(|| (Vec::new(), None));

        out.push(LibraryGameDto {
            game_id: gid,
            name: inst.name,
            slug: inst.slug,
            description: String::new(),
            system_requirements: String::new(),
            uploader_name: None,
            uploaded_by: None,
            uploader_pfp: None,
            created_at: inst.installed_at.to_rfc3339(),
            status: "ready".to_string(),
            total_size: inst.total_size,
            download_count: 0,
            has_cover: true,
            has_overlay: inst.last_synced_at.is_some(),
            installed: true,
            install_dir: Some(inst.install_dir.to_string_lossy().to_string()),
            executable_rel_path: inst.executable_rel_path,
            discovered_executables,
            binary_kind,
            proton_override: inst.proton_override,
            last_synced_at: inst.last_synced_at,
            pending_sync: pending_set.contains(&gid),
            playtime,
        });
    }

    Ok(out)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_game_cover_data_url(
    state: State<'_, AppState>,
    game_id: i64,
) -> Result<Option<String>, String> {
    match state.core.fetch_cached_cover(game_id).await {
        Ok((bytes, mime)) => {
            let url = tokio::task::spawn_blocking(move || {
                let encoded = BASE64.encode(&bytes);
                format!("data:{mime};base64,{encoded}")
            })
            .await
            .map_err(|e| format!("encode cover failed: {e}"))?;
            Ok(Some(url))
        }
        Err(CoreError::Api(ApiError::Http { status: 404, .. })) => Ok(None),
        Err(CoreError::Api(ApiError::Network(_))) => Ok(None),
        Err(e) => Err(err_str(e)),
    }
}

#[tauri::command(rename_all = "snake_case")]
pub async fn start_game_download(
    state: State<'_, AppState>,
    game_id: i64,
    fresh: bool,
) -> Result<String, String> {
    state
        .core
        .start_game_download(game_id, fresh)
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn pause_download(state: State<'_, AppState>, job_id: String) -> Result<bool, String> {
    Ok(state.core.pause_download(&job_id).await)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn resume_download(state: State<'_, AppState>, job_id: String) -> Result<bool, String> {
    Ok(state.core.resume_download(&job_id).await)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn cancel_download(state: State<'_, AppState>, job_id: String) -> Result<bool, String> {
    Ok(state.core.cancel_download(&job_id).await)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_bandwidth_limit(
    state: State<'_, AppState>,
    bytes_per_sec: u64,
) -> Result<u64, String> {
    state
        .bandwidth_limit_bps
        .store(bytes_per_sec, Ordering::Relaxed);
    state.core.set_bandwidth_limit(bytes_per_sec).await;
    Ok(bytes_per_sec)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_downloads(state: State<'_, AppState>) -> Result<Vec<DownloadJobSnapshot>, String> {
    Ok(state.core.list_downloads().await)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn launch_game(state: State<'_, AppState>, game_id: i64) -> Result<bool, String> {
    let profile = state
        .core
        .active_profile()
        .await
        .ok_or_else(|| "no active profile selected".to_string())?;
    let installed = state
        .core
        .db()
        .get_installed_game(&profile.id, game_id)
        .map_err(err_str)?;
    if installed.is_none() {
        return Err(format!("game {game_id} is not installed"));
    }

    let core = Arc::clone(&state.core);
    tokio::spawn(async move {
        let opts = LaunchOptions {
            auto_sync_on_close: true,
            ..LaunchOptions::default()
        };
        if let Err(err) = core.launch_game(game_id, opts).await {
            tracing::warn!(game_id, error = %err, "game launch failed");
        }
    });

    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn sync_game_now(
    state: State<'_, AppState>,
    game_id: i64,
    force: bool,
) -> Result<SyncReport, String> {
    state
        .core
        .sync_game_now(game_id, force)
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn uninstall_game(state: State<'_, AppState>, game_id: i64) -> Result<bool, String> {
    state.core.uninstall_game(game_id).await.map_err(err_str)?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_game_executable(
    state: State<'_, AppState>,
    game_id: i64,
    rel_path: Option<String>,
) -> Result<bool, String> {
    state
        .core
        .set_game_executable(game_id, rel_path)
        .await
        .map_err(err_str)?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn set_game_proton(
    state: State<'_, AppState>,
    game_id: i64,
    proton_name: Option<String>,
) -> Result<bool, String> {
    state
        .core
        .set_game_proton(game_id, proton_name)
        .await
        .map_err(err_str)?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_proton_installations(
    state: State<'_, AppState>,
) -> Result<Vec<ProtonInstallation>, String> {
    let core = Arc::clone(&state.core);
    tokio::task::spawn_blocking(move || core.detect_proton())
        .await
        .map_err(|e| format!("proton scan failed: {e}"))
}

#[tauri::command(rename_all = "snake_case")]
pub async fn fetch_latest_ge_proton() -> Result<GeProtonRelease, String> {
    opensync_proton::fetch_latest_ge_proton()
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn download_ge_proton(
    state: State<'_, AppState>,
    release: GeProtonRelease,
) -> Result<String, String> {
    state
        .core
        .start_ge_proton_download(release)
        .await
        .map_err(err_str)
}

// ---------- Profile name + game uploads ----------

#[tauri::command(rename_all = "snake_case")]
pub async fn change_username(
    state: State<'_, AppState>,
    username: String,
) -> Result<Profile, String> {
    state.core.change_username(&username).await.map_err(err_str)
}

/// Opens a native directory picker using whichever desktop dialog tool is installed.
/// Returns `None` when the dialog is unavailable or the user cancels.
#[tauri::command(rename_all = "snake_case")]
pub async fn pick_upload_folder() -> Result<Option<String>, String> {
    tokio::task::spawn_blocking(|| {
        fn try_dialog(program: &str, args: &[&str]) -> Option<String> {
            let output = std::process::Command::new(program).args(args).output().ok()?;
            if !output.status.success() {
                return None;
            }
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }

        #[cfg(target_os = "windows")]
        let picked = try_dialog(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; $d = New-Object System.Windows.Forms.FolderBrowserDialog; $d.Description = 'Choose game folder'; if ($d.ShowDialog() -eq 'OK') { Write-Output $d.SelectedPath }",
            ],
        );

        #[cfg(not(target_os = "windows"))]
        let picked = try_dialog(
            "zenity",
            &["--file-selection", "--directory", "--title=Choose game folder"],
        )
        .or_else(|| try_dialog("kdialog", &["--getexistingdirectory", "/home"]))
        .or_else(|| {
            try_dialog(
                "yad",
                &["--file-selection", "--directory", "--title=Choose game folder"],
            )
        });

        picked
    })
    .await
    .map_err(|e| format!("folder dialog failed: {e}"))
}

fn collect_upload_files(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, u64)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if ft.is_dir() {
            collect_upload_files(&path, root, out);
        } else if ft.is_file() && !name.ends_with(".part") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            out.push((rel, size));
        }
    }
}

#[tauri::command(rename_all = "snake_case")]
pub async fn scan_upload_folder(path: String) -> Result<FolderScanDto, String> {
    let raw = path.trim().to_string();
    if raw.is_empty() {
        return Err("Folder path is empty".to_string());
    }
    tokio::task::spawn_blocking(move || {
        let root = expand_home(&raw);
        let mut files: Vec<(String, u64)> = Vec::new();
        collect_upload_files(&root, &root, &mut files);
        files.sort_by(|a, b| b.1.cmp(&a.1));
        let total_bytes = files.iter().map(|(_, size)| *size).sum();
        let top_files = files
            .iter()
            .take(8)
            .map(|(rel, _)| rel.clone())
            .collect();
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().replace('_', " "))
            .unwrap_or_else(|| "Game".to_string());
        Ok(FolderScanDto {
            file_count: files.len(),
            total_bytes,
            name,
            top_files,
        })
    })
    .await
    .map_err(|e| format!("folder scan task failed: {e}"))?
}

fn is_supported_image_ext(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png"))
        .unwrap_or(false)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn inspect_dropped_paths(paths: Vec<String>) -> Result<Vec<DroppedPathDto>, String> {
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::with_capacity(paths.len());
        for raw in paths {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let p = expand_home(trimmed);
            let Ok(meta) = std::fs::metadata(&p) else {
                continue;
            };
            let is_dir = meta.is_dir();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned());
            let is_image = !is_dir && is_supported_image_ext(&p);
            out.push(DroppedPathDto {
                path: p.to_string_lossy().into_owned(),
                name,
                is_dir,
                is_image,
                size: if is_dir { 0 } else { meta.len() },
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| format!("inspect dropped paths failed: {e}"))?
}

#[tauri::command(rename_all = "snake_case")]
pub async fn read_local_image(path: String) -> Result<LocalImageDto, String> {
    const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
    let raw = path.trim().to_string();
    if raw.is_empty() {
        return Err("Image path is empty".to_string());
    }
    tokio::task::spawn_blocking(move || {
        let p = expand_home(&raw);
        let meta = std::fs::metadata(&p).map_err(|e| format!("cannot access image: {e}"))?;
        if !meta.is_file() {
            return Err("Dropped item is not an image file".to_string());
        }
        if meta.len() == 0 || meta.len() > MAX_IMAGE_BYTES {
            return Err("Image must be between 1 byte and 8 MB".to_string());
        }
        let bytes = std::fs::read(&p).map_err(|e| format!("failed to read image: {e}"))?;
        let content_type = if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
            "image/png"
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "image/jpeg"
        } else {
            return Err("Only JPG and PNG images are supported".to_string());
        };
        let encoded = BASE64.encode(&bytes);
        let data_url = format!("data:{content_type};base64,{encoded}");
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        Ok(LocalImageDto {
            base64: encoded,
            content_type: content_type.to_string(),
            data_url,
            name,
        })
    })
    .await
    .map_err(|e| format!("read local image failed: {e}"))?
}

#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub async fn start_game_upload(
    state: State<'_, AppState>,
    name: String,
    description: String,
    system_requirements: String,
    folder: Option<String>,
    cover: Option<String>,
    cover_content_type: Option<String>,
) -> Result<String, String> {
    let cover_bytes = match (cover, cover_content_type) {
        (Some(b64), Some(ct)) => {
            use base64::Engine as _;
            let encoded = b64.split_once(',').map(|(_, rest)| rest).unwrap_or(&b64);
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|e| format!("invalid cover image: {e}"))?;
            if bytes.is_empty() || bytes.len() > 8 * 1024 * 1024 {
                return Err("cover image must be between 1 byte and 8 MB".to_string());
            }
            Some((bytes, ct))
        }
        _ => None,
    };
    state
        .core
        .start_game_upload(
            &name,
            &description,
            &system_requirements,
            folder.as_deref(),
            cover_bytes,
        )
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub async fn upload_game_chunk(
    state: State<'_, AppState>,
    job_id: String,
    rel_path: String,
    index: usize,
    total: usize,
    size: u64,
    chunk: Option<Vec<u8>>,
    chunk_base64: Option<String>,
) -> Result<bool, String> {
    let core = Arc::clone(&state.core);
    tokio::task::spawn_blocking(move || {
        let bytes = if let Some(b64) = chunk_base64 {
            BASE64
                .decode(b64.trim())
                .map_err(|e| format!("invalid chunk base64: {e}"))?
        } else {
            chunk.unwrap_or_default()
        };
        core.stage_upload_chunk(&job_id, &rel_path, index, total, size, &bytes)
            .map_err(err_str)
    })
    .await
    .map_err(|e| format!("upload_game_chunk task failed: {e}"))?
}

#[tauri::command(rename_all = "snake_case")]
pub async fn finalize_dropped_upload(
    state: State<'_, AppState>,
    job_id: String,
) -> Result<bool, String> {
    state
        .core
        .finalize_dropped_upload(&job_id)
        .await
        .map_err(err_str)?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn pause_upload(state: State<'_, AppState>, job_id: String) -> Result<bool, String> {
    Ok(state.core.pause_upload(&job_id).await)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn resume_upload(state: State<'_, AppState>, job_id: String) -> Result<bool, String> {
    Ok(state.core.resume_upload(&job_id).await)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn cancel_upload(state: State<'_, AppState>, job_id: String) -> Result<bool, String> {
    Ok(state.core.cancel_upload(&job_id).await)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_uploads(state: State<'_, AppState>) -> Result<Vec<UploadJobSnapshot>, String> {
    Ok(state.core.list_uploads().await)
}

// ---------- Activity logs, force-quit recovery & safe shutdown ----------

#[tauri::command(rename_all = "snake_case")]
pub async fn list_activity_logs(
    state: State<'_, AppState>,
    limit: Option<usize>,
) -> Result<Vec<ActivityLogEntry>, String> {
    state
        .core
        .list_activity_logs(limit.unwrap_or(80))
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn clear_activity_logs(state: State<'_, AppState>) -> Result<bool, String> {
    state.core.clear_activity_logs().await.map_err(err_str)?;
    Ok(true)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn list_interrupted_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<InterruptedSessionDto>, String> {
    state
        .core
        .detect_interrupted_sessions()
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn resolve_interrupted_session(
    state: State<'_, AppState>,
    game_id: i64,
    sync_saves: bool,
    force: Option<bool>,
) -> Result<Option<SyncReport>, String> {
    state
        .core
        .resolve_interrupted_session(game_id, sync_saves, force.unwrap_or(false))
        .await
        .map_err(err_str)
}

#[tauri::command(rename_all = "snake_case")]
pub async fn get_shutdown_status(state: State<'_, AppState>) -> Result<ShutdownStatusDto, String> {
    let core = Arc::clone(&state.core);
    tokio::task::spawn_blocking(move || Ok(core.shutdown_status()))
        .await
        .map_err(|e| format!("shutdown status failed: {e}"))?
}

#[tauri::command(rename_all = "snake_case")]
pub async fn confirm_safe_shutdown(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    force_while_playing: bool,
) -> Result<ShutdownResultDto, String> {
    let result = state
        .core
        .perform_safe_shutdown(Duration::from_secs(10), force_while_playing)
        .await
        .map_err(err_str)?;

    if result.completed {
        state.allow_window_close.store(true, Ordering::SeqCst);
        let handle = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(120)).await;
            handle.exit(0);
        });
    }

    Ok(result)
}
