use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use launcher_core::{
    ConnectionStateDto, CoreError, DownloadJobSnapshot, LauncherCore, ReconnectPolicy,
};
use launcher_db::{InstalledGame, PendingSync, PlaytimeSummary, Profile};
use opensync_api::{ApiError, Game};
use opensync_proton::{
    BinaryKind, DiscoveredExecutable, GeProtonRelease, ProtonInstallation, detect_binary_kind,
    discover_executables,
};
use opensync_runner::LaunchOptions;
use opensync_sync::SyncReport;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::State;

pub struct AppState {
    pub core: Arc<LauncherCore>,
    pub bandwidth_limit_bps: AtomicU64,
}

impl AppState {
    pub fn new(core: Arc<LauncherCore>) -> Self {
        Self {
            core,
            bandwidth_limit_bps: AtomicU64::new(0),
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
}

#[derive(Debug, Clone, Serialize)]
pub struct ReconnectOutcomeDto {
    pub attempts: u32,
    pub flushed_syncs: usize,
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
    let proton_installations = state.core.detect_proton();
    let pending_syncs = state.core.list_pending_syncs().await.unwrap_or_default();
    let downloads = state.core.list_downloads().await;
    let bandwidth_limit_bps = state.bandwidth_limit_bps.load(Ordering::Relaxed);

    Ok(BootstrapStateDto {
        active_profile,
        profiles,
        connection_state,
        proton_installations,
        pending_syncs,
        downloads,
        bandwidth_limit_bps,
    })
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

    let mut out = Vec::with_capacity(remote_games.len() + installed_map.len());
    let mut seen_ids = HashSet::new();

    for remote in remote_games {
        seen_ids.insert(remote.id);
        let inst = installed_map.remove(&remote.id);
        let playtime = state
            .core
            .db()
            .playtime_summary(&profile.id, remote.id)
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
        let playtime = state
            .core
            .db()
            .playtime_summary(&profile.id, gid)
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
            let encoded = BASE64.encode(&bytes);
            Ok(Some(format!("data:{mime};base64,{encoded}")))
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
    Ok(state.core.detect_proton())
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
