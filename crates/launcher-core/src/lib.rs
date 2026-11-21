use chrono::Utc;
use launcher_db::{
    DbError, InstalledGame, LauncherDb, PendingSync, PlaytimeSummary, Profile, SecretStore,
};
use opensync_api::{ApiError, Game, OpenSyncClient};
use opensync_proton::{
    GeProtonRelease, ProtonError, ProtonInstallation, default_proton_search_roots,
    detect_proton_in_roots, discover_executables, download_and_install_ge_proton_with_control,
};
use opensync_runner::{
    LaunchOptions, PostExitSyncStatus, RunnerError, SessionOutcome, launch_and_track,
};
use opensync_sync::{SyncError, SyncReport, sync_game_saves};
use opensync_transfer::{
    DownloadControl, DownloadOutcome, SpeedSample, TransferError, download_and_extract_game,
    safe_rel_path,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock as StdRwLock};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("db error: {0}")]
    Db(#[from] DbError),
    #[error("api error: {0}")]
    Api(#[from] ApiError),
    #[error("transfer error: {0}")]
    Transfer(#[from] TransferError),
    #[error("sync error: {0}")]
    Sync(#[from] SyncError),
    #[error("runner error: {0}")]
    Runner(#[from] RunnerError),
    #[error("proton error: {0}")]
    Proton(#[from] ProtonError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("no active profile selected")]
    NoActiveProfile,
    #[error("profile not found: {0}")]
    ProfileNotFound(String),
    #[error("game {0} is not installed for the active profile")]
    GameNotInstalled(i64),
    #[error("reconnect failed after {attempts} attempts: {last_error}")]
    ReconnectExhausted { attempts: u32, last_error: String },
}

pub type CoreResult<T> = Result<T, CoreError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConnectionStateDto {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting { attempt: u32 },
}

impl From<&ConnectionState> for ConnectionStateDto {
    fn from(s: &ConnectionState) -> Self {
        match s {
            ConnectionState::Disconnected => Self::Disconnected,
            ConnectionState::Connecting => Self::Connecting,
            ConnectionState::Connected => Self::Connected,
            ConnectionState::Reconnecting { attempt } => Self::Reconnecting { attempt: *attempt },
        }
    }
}

impl From<ConnectionState> for ConnectionStateDto {
    fn from(s: ConnectionState) -> Self {
        Self::from(&s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DownloadKind {
    Game { game_id: i64, fresh: bool },
    Proton { tag_name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DownloadStatus {
    Queued,
    Downloading,
    Verifying,
    Paused,
    Completed,
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadJobSnapshot {
    pub id: String,
    pub name: String,
    pub kind: DownloadKind,
    pub status: DownloadStatus,
    pub bytes_downloaded: u64,
    pub total_bytes: u64,
    pub bytes_per_sec: f64,
    pub files_extracted: usize,
    pub total_files: usize,
    pub current_file: Option<String>,
    pub speed_samples: Vec<SpeedSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LauncherEvent {
    ConnectionChanged {
        state: ConnectionStateDto,
    },
    DownloadUpdated {
        job: DownloadJobSnapshot,
    },
    GameSessionStarted {
        game_id: i64,
        name: String,
    },
    GameSessionEnded {
        game_id: i64,
        duration_secs: u64,
        synced: bool,
        error: Option<String>,
    },
    ProtonInstalled {
        installation: ProtonInstallation,
    },
}

#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub max_attempts: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(15),
            max_attempts: 10,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReconnectOutcome {
    pub attempts: u32,
    pub flushed_syncs: usize,
}

#[derive(Debug, Clone)]
pub struct LauncherPaths {
    pub library_root: PathBuf,
    pub proton_dir: PathBuf,
    pub prefixes_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl LauncherPaths {
    pub fn under(root: impl AsRef<Path>) -> Self {
        let r = root.as_ref();
        Self {
            library_root: r.join("games"),
            proton_dir: r.join("proton"),
            prefixes_dir: r.join("prefixes"),
            cache_dir: r.join("cache"),
        }
    }
}

struct ActiveSession {
    profile: Profile,
    client: OpenSyncClient,
    state: ConnectionState,
}

struct ActiveDownloadEntry {
    seq: u64,
    snapshot: DownloadJobSnapshot,
    control: DownloadControl,
}

pub struct LauncherCore {
    db: LauncherDb,
    secrets: Arc<dyn SecretStore>,
    paths: LauncherPaths,
    active: RwLock<Option<ActiveSession>>,
    events: broadcast::Sender<LauncherEvent>,
    downloads: StdRwLock<HashMap<String, ActiveDownloadEntry>>,
    bandwidth_limit: AtomicU64,
    next_job_seq: AtomicU64,
}

impl LauncherCore {
    pub fn new(db: LauncherDb, secrets: Arc<dyn SecretStore>, paths: LauncherPaths) -> Self {
        let (events, _) = broadcast::channel(512);
        Self {
            db,
            secrets,
            paths,
            active: RwLock::new(None),
            events,
            downloads: StdRwLock::new(HashMap::new()),
            bandwidth_limit: AtomicU64::new(0),
            next_job_seq: AtomicU64::new(1),
        }
    }

    pub fn db(&self) -> &LauncherDb {
        &self.db
    }

    pub fn paths(&self) -> &LauncherPaths {
        &self.paths
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<LauncherEvent> {
        self.events.subscribe()
    }

    fn emit(&self, event: LauncherEvent) {
        let _ = self.events.send(event);
    }

    fn emit_connection_state(&self, state: &ConnectionState) {
        self.emit(LauncherEvent::ConnectionChanged {
            state: ConnectionStateDto::from(state),
        });
    }

    pub async fn connection_state(&self) -> ConnectionState {
        self.active
            .read()
            .await
            .as_ref()
            .map(|a| a.state.clone())
            .unwrap_or(ConnectionState::Disconnected)
    }

    pub async fn active_profile(&self) -> Option<Profile> {
        self.active.read().await.as_ref().map(|a| a.profile.clone())
    }

    pub fn list_profiles(&self) -> CoreResult<Vec<Profile>> {
        Ok(self.db.list_profiles()?)
    }

    /// Signs into an OpenSync instance, stores the profile and cookie, remembers it as active,
    /// and connects immediately.
    pub async fn connect_with_password(
        &self,
        label: &str,
        server_url: &str,
        username: &str,
        password: &str,
    ) -> CoreResult<Profile> {
        let anon = OpenSyncClient::new(server_url, None)?;
        let (user, cookie) = anon.login(username, password).await?;
        let profile =
            self.db
                .upsert_profile(label, anon.base_url(), &user.username, user.id, &user.role)?;
        self.secrets.set(&profile.id, &cookie)?;
        self.db.set_active_profile_id(Some(&profile.id))?;

        let client = anon.with_cookie(Some(cookie));
        *self.active.write().await = Some(ActiveSession {
            profile: profile.clone(),
            client,
            state: ConnectionState::Connected,
        });
        self.emit_connection_state(&ConnectionState::Connected);
        Ok(profile)
    }

    /// Registers a new account on an OpenSync instance and persists the remembered profile.
    pub async fn register_and_connect(
        &self,
        label: &str,
        server_url: &str,
        username: &str,
        password: &str,
    ) -> CoreResult<Profile> {
        let anon = OpenSyncClient::new(server_url, None)?;
        let (user, cookie) = anon.register(username, password).await?;
        let profile =
            self.db
                .upsert_profile(label, anon.base_url(), &user.username, user.id, &user.role)?;
        self.secrets.set(&profile.id, &cookie)?;
        self.db.set_active_profile_id(Some(&profile.id))?;

        let client = anon.with_cookie(Some(cookie));
        *self.active.write().await = Some(ActiveSession {
            profile: profile.clone(),
            client,
            state: ConnectionState::Connected,
        });
        self.emit_connection_state(&ConnectionState::Connected);
        Ok(profile)
    }

    /// Restores the last active profile saved in SQLite + `SecretStore` across launcher restarts.
    /// If the server is online, verifies the remembered session cookie and marks `Connected`.
    /// If the server is temporarily offline, loads the profile in `Disconnected` state so
    /// offline play and auto-reconnect work seamlessly.
    pub async fn restore_remembered_profile(&self) -> CoreResult<Option<Profile>> {
        let Some(active_id) = self.db.get_active_profile_id()? else {
            return Ok(None);
        };
        match self.switch_profile(&active_id).await {
            Ok(profile) => Ok(Some(profile)),
            Err(CoreError::ProfileNotFound(_)) => {
                self.db.set_active_profile_id(None)?;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Switches to an existing saved profile, restoring its remembered session cookie and
    /// scoping installed games / sync queue / screen time to that profile.
    pub async fn switch_profile(&self, profile_id: &str) -> CoreResult<Profile> {
        let profile = self
            .db
            .get_profile(profile_id)?
            .ok_or_else(|| CoreError::ProfileNotFound(profile_id.to_string()))?;
        let cookie = self.secrets.get(&profile.id)?;
        let client = OpenSyncClient::new(&profile.server_url, cookie)?;
        self.db.set_active_profile_id(Some(&profile.id))?;

        let state = match client.me().await {
            Ok(user) => {
                let updated = self.db.upsert_profile(
                    &profile.label,
                    &profile.server_url,
                    &user.username,
                    user.id,
                    &user.role,
                )?;
                *self.active.write().await = Some(ActiveSession {
                    profile: updated.clone(),
                    client,
                    state: ConnectionState::Connected,
                });
                self.emit_connection_state(&ConnectionState::Connected);
                let _ = self.flush_pending_syncs().await;
                return Ok(updated);
            }
            Err(ApiError::Network(_)) => ConnectionState::Disconnected,
            Err(ApiError::Unauthorized) => ConnectionState::Disconnected,
            Err(_) => ConnectionState::Disconnected,
        };

        *self.active.write().await = Some(ActiveSession {
            profile: profile.clone(),
            client,
            state: state.clone(),
        });
        self.emit_connection_state(&state);
        Ok(profile)
    }

    /// Deletes a saved profile, removes its stored cookie, cleans up its local library directory
    /// and cached covers, and disconnects if it was the active profile.
    pub async fn delete_profile(&self, profile_id: &str) -> CoreResult<()> {
        if self.db.get_profile(profile_id)?.is_none() {
            return Err(CoreError::ProfileNotFound(profile_id.to_string()));
        }

        self.secrets.delete(profile_id)?;
        self.db.delete_profile(profile_id)?;

        let profile_lib_dir = self.paths.library_root.join(profile_id);
        if profile_lib_dir.exists() {
            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&profile_lib_dir))
                .await
                .map_err(|e| std::io::Error::other(format!("cleanup task panicked: {e}")))??;
        }

        // Best-effort cleanup of cached covers belonging to this profile.
        let covers_dir = self.paths.cache_dir.join("covers");
        let prefix = format!("{profile_id}_");
        if covers_dir.is_dir() {
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(entries) = std::fs::read_dir(&covers_dir) {
                    for entry in entries.filter_map(Result::ok) {
                        if let Some(name) = entry.file_name().to_str()
                            && name.starts_with(&prefix)
                        {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
            })
            .await;
        }

        let cleared_active = {
            let mut guard = self.active.write().await;
            if guard.as_ref().is_some_and(|a| a.profile.id == profile_id) {
                *guard = None;
                true
            } else {
                false
            }
        };
        if cleared_active {
            self.emit_connection_state(&ConnectionState::Disconnected);
        }

        Ok(())
    }

    /// Checks whether the active profile's server is currently reachable and marks the
    /// connection `Disconnected` if a network failure occurs.
    pub async fn probe_connection(&self) -> CoreResult<bool> {
        let client = self.active_client().await?;
        match client.health().await {
            Ok(h) if h.ok => {
                let changed = {
                    let mut guard = self.active.write().await;
                    if let Some(a) = guard.as_mut() {
                        let c = a.state != ConnectionState::Connected;
                        a.state = ConnectionState::Connected;
                        c
                    } else {
                        false
                    }
                };
                if changed {
                    self.emit_connection_state(&ConnectionState::Connected);
                }
                Ok(true)
            }
            _ => {
                let changed = {
                    let mut guard = self.active.write().await;
                    if let Some(a) = guard.as_mut() {
                        let c = a.state != ConnectionState::Disconnected;
                        a.state = ConnectionState::Disconnected;
                        c
                    } else {
                        false
                    }
                };
                if changed {
                    self.emit_connection_state(&ConnectionState::Disconnected);
                }
                Ok(false)
            }
        }
    }

    /// Automatically reconnects to the active profile's OpenSync server using exponential backoff.
    /// Once reachable, verifies the stored session with `/api/auth/me` and automatically flushes
    /// any offline queued save syncs.
    pub async fn reconnect_with_backoff(
        &self,
        policy: ReconnectPolicy,
    ) -> CoreResult<ReconnectOutcome> {
        let (profile_id, client) = {
            let guard = self.active.read().await;
            let active = guard.as_ref().ok_or(CoreError::NoActiveProfile)?;
            (active.profile.id.clone(), active.client.clone())
        };

        let mut delay = policy.initial_delay;
        let mut last_error = String::new();

        for attempt in 1..=policy.max_attempts {
            let reconnecting = ConnectionState::Reconnecting { attempt };
            if let Some(a) = self.active.write().await.as_mut() {
                a.state = reconnecting.clone();
            }
            self.emit_connection_state(&reconnecting);

            match client.health().await {
                Ok(h) if h.ok => match client.me().await {
                    Ok(_) => {
                        let _ = self.db.touch_profile_connected(&profile_id);
                        if let Some(a) = self.active.write().await.as_mut() {
                            a.state = ConnectionState::Connected;
                        }
                        self.emit_connection_state(&ConnectionState::Connected);
                        let flushed = self.flush_pending_syncs().await.unwrap_or(0);
                        return Ok(ReconnectOutcome {
                            attempts: attempt,
                            flushed_syncs: flushed,
                        });
                    }
                    Err(ApiError::Unauthorized) => {
                        if let Some(a) = self.active.write().await.as_mut() {
                            a.state = ConnectionState::Disconnected;
                        }
                        self.emit_connection_state(&ConnectionState::Disconnected);
                        return Err(CoreError::Api(ApiError::Unauthorized));
                    }
                    Err(e) => {
                        last_error = e.to_string();
                    }
                },
                Ok(_) => {
                    last_error = "server health returned ok=false".to_string();
                }
                Err(e) => {
                    last_error = e.to_string();
                }
            }

            if attempt < policy.max_attempts {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(policy.max_delay);
            }
        }

        if let Some(a) = self.active.write().await.as_mut() {
            a.state = ConnectionState::Disconnected;
        }
        self.emit_connection_state(&ConnectionState::Disconnected);
        Err(CoreError::ReconnectExhausted {
            attempts: policy.max_attempts,
            last_error,
        })
    }

    /// Lists games from the active OpenSync server.
    pub async fn fetch_remote_games(&self) -> CoreResult<Vec<Game>> {
        let client = self.active_client().await?;
        match client.list_games().await {
            Ok(games) => Ok(games),
            Err(ApiError::Network(e)) => {
                let changed = {
                    let mut guard = self.active.write().await;
                    if let Some(a) = guard.as_mut() {
                        let c = a.state != ConnectionState::Disconnected;
                        a.state = ConnectionState::Disconnected;
                        c
                    } else {
                        false
                    }
                };
                if changed {
                    self.emit_connection_state(&ConnectionState::Disconnected);
                }
                Err(CoreError::Api(ApiError::Network(e)))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Returns remembered downloaded games scoped to the currently active profile.
    pub async fn list_downloaded_games(&self) -> CoreResult<Vec<InstalledGame>> {
        let profile = self
            .active_profile()
            .await
            .ok_or(CoreError::NoActiveProfile)?;
        Ok(self.db.list_installed_games(&profile.id)?)
    }

    /// Fetches a game's cover art, serving it from `paths.cache_dir/covers/` when already cached
    /// so cover images remain available even when the server is offline.
    pub async fn fetch_cached_cover(&self, game_id: i64) -> CoreResult<(Vec<u8>, String)> {
        let (profile, client) = self.active_profile_and_client().await?;
        let covers_dir = self.paths.cache_dir.join("covers");
        let base = format!("{}_{}", profile.id, game_id);
        let mime_path = covers_dir.join(format!("{base}.mime"));

        if mime_path.is_file()
            && let Ok(ct_raw) = std::fs::read_to_string(&mime_path)
        {
            let ct = ct_raw.trim().to_string();
            let data_path = covers_dir.join(format!("{base}.{}", ext_for_mime(&ct)));
            if let Ok(bytes) = std::fs::read(&data_path)
                && !bytes.is_empty()
            {
                return Ok((bytes, ct));
            }
        }

        let (bytes, content_type) = client.fetch_cover(game_id).await?;
        let dir_clone = covers_dir.clone();
        let base_clone = base.clone();
        let bytes_clone = bytes.clone();
        let ct_clone = content_type.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&dir_clone)?;
            let data_path = dir_clone.join(format!("{base_clone}.{}", ext_for_mime(&ct_clone)));
            let mime_file = dir_clone.join(format!("{base_clone}.mime"));
            std::fs::write(&data_path, &bytes_clone)?;
            std::fs::write(&mime_file, &ct_clone)?;
            Ok(())
        })
        .await
        .map_err(|e| std::io::Error::other(format!("cover cache write panicked: {e}")))??;

        Ok((bytes, content_type))
    }

    /// Downloads a game from the active OpenSync instance using high-speed streaming extraction,
    /// auto-discovers its launch executable, and persists the installation in SQLite.
    pub async fn download_game(
        &self,
        game_id: i64,
        fresh: bool,
        control: DownloadControl,
    ) -> CoreResult<(InstalledGame, DownloadOutcome)> {
        let (profile, client) = self.active_profile_and_client().await?;
        let detail = client.get_game(game_id).await?;
        let verify_clean = fresh || !detail.overlay.has_overlay;

        let install_dir = self
            .paths
            .library_root
            .join(&profile.id)
            .join(&detail.game.slug);

        let outcome = download_and_extract_game(
            &client,
            game_id,
            &install_dir,
            fresh,
            verify_clean,
            control,
        )
        .await?;

        let installed = self.record_installed_game(
            &profile.id,
            game_id,
            &detail.game.name,
            &detail.game.slug,
            &install_dir,
            &outcome,
            !fresh && detail.overlay.has_overlay,
        )?;
        Ok((installed, outcome))
    }

    /// Starts a managed background game download tracked by the Download Manager.
    /// Emits `LauncherEvent::DownloadUpdated` throughout the lifecycle until `Completed`,
    /// `Failed`, or `Cancelled`.
    pub async fn start_game_download(
        self: &Arc<Self>,
        game_id: i64,
        fresh: bool,
    ) -> CoreResult<String> {
        let (profile, client) = self.active_profile_and_client().await?;
        let detail = client.get_game(game_id).await?;
        let verify_clean = fresh || !detail.overlay.has_overlay;
        let has_overlay = detail.overlay.has_overlay;
        let game_name = detail.game.name.clone();
        let game_slug = detail.game.slug.clone();
        let initial_total = detail.game.total_size;

        let install_dir = self
            .paths
            .library_root
            .join(&profile.id)
            .join(&game_slug);

        let job_id = Uuid::new_v4().to_string();
        let seq = self.next_job_seq.fetch_add(1, Ordering::Relaxed);
        let control = DownloadControl::new(initial_total, 0);
        control.set_bandwidth_limit(self.bandwidth_limit.load(Ordering::SeqCst));

        let initial_snapshot = DownloadJobSnapshot {
            id: job_id.clone(),
            name: game_name.clone(),
            kind: DownloadKind::Game { game_id, fresh },
            status: DownloadStatus::Queued,
            bytes_downloaded: 0,
            total_bytes: initial_total,
            bytes_per_sec: 0.0,
            files_extracted: 0,
            total_files: 0,
            current_file: None,
            speed_samples: Vec::new(),
        };

        {
            let mut map = self.downloads.write().unwrap();
            map.insert(
                job_id.clone(),
                ActiveDownloadEntry {
                    seq,
                    snapshot: initial_snapshot.clone(),
                    control: control.clone(),
                },
            );
        }
        self.emit(LauncherEvent::DownloadUpdated {
            job: initial_snapshot,
        });

        let core = Arc::clone(self);
        let jid = job_id.clone();
        tokio::spawn(async move {
            // Transition Queued -> Downloading (unless already paused or cancelled).
            if control.is_cancelled() {
                core.finalize_cancelled(&jid);
                return;
            }
            if !control.is_paused() {
                core.update_download_snapshot(&jid, |snap| {
                    snap.status = DownloadStatus::Downloading;
                });
            }

            // Periodic progress forwarder while download_and_extract_game runs.
            let poll_core = Arc::clone(&core);
            let poll_jid = jid.clone();
            let poll_ctrl = control.clone();
            let ticker = tokio::spawn(async move {
                let mut last_bytes = u64::MAX;
                let mut last_files = usize::MAX;
                let mut last_file: Option<String> = None;
                let mut last_samples = usize::MAX;
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    let tp = poll_ctrl.snapshot();
                    let changed = tp.bytes_downloaded != last_bytes
                        || tp.files_extracted != last_files
                        || tp.current_file != last_file
                        || tp.speed_samples.len() != last_samples;
                    if changed {
                        last_bytes = tp.bytes_downloaded;
                        last_files = tp.files_extracted;
                        last_file = tp.current_file.clone();
                        last_samples = tp.speed_samples.len();
                        let is_paused = poll_ctrl.is_paused();
                        poll_core.update_download_snapshot(&poll_jid, |snap| {
                            if !matches!(
                                snap.status,
                                DownloadStatus::Cancelled
                                    | DownloadStatus::Completed
                                    | DownloadStatus::Failed { .. }
                            ) {
                                snap.status = if is_paused {
                                    DownloadStatus::Paused
                                } else {
                                    DownloadStatus::Downloading
                                };
                            }
                            snap.bytes_downloaded = tp.bytes_downloaded;
                            if tp.total_bytes > 0 {
                                snap.total_bytes = tp.total_bytes;
                            }
                            snap.bytes_per_sec = if is_paused { 0.0 } else { tp.bytes_per_sec };
                            snap.files_extracted = tp.files_extracted;
                            snap.total_files = tp.total_files;
                            snap.current_file = tp.current_file;
                            snap.speed_samples = tp.speed_samples;
                        });
                    }
                }
            });

            let result = download_and_extract_game(
                &client,
                game_id,
                &install_dir,
                fresh,
                verify_clean,
                control.clone(),
            )
            .await;

            ticker.abort();
            let _ = ticker.await;

            let final_tp = control.snapshot();
            match result {
                Ok(outcome) => {
                    match core.record_installed_game(
                        &profile.id,
                        game_id,
                        &game_name,
                        &game_slug,
                        &install_dir,
                        &outcome,
                        !fresh && has_overlay,
                    ) {
                        Ok(_) => {
                            core.update_download_snapshot(&jid, |snap| {
                                snap.status = DownloadStatus::Completed;
                                let final_total = outcome.total_bytes.max(final_tp.total_bytes);
                                snap.total_bytes = final_total;
                                snap.bytes_downloaded = final_total.max(final_tp.bytes_downloaded);
                                snap.bytes_per_sec = outcome.avg_bytes_per_sec;
                                snap.files_extracted = outcome.files.len();
                                snap.total_files = outcome.files.len();
                                snap.current_file = None;
                                snap.speed_samples = final_tp.speed_samples;
                            });
                        }
                        Err(err) => {
                            core.update_download_snapshot(&jid, |snap| {
                                snap.status = DownloadStatus::Failed {
                                    error: err.to_string(),
                                };
                                snap.bytes_per_sec = 0.0;
                            });
                        }
                    }
                }
                Err(TransferError::Cancelled) => {
                    core.finalize_cancelled(&jid);
                }
                Err(err) => {
                    if control.is_cancelled() {
                        core.finalize_cancelled(&jid);
                    } else {
                        core.update_download_snapshot(&jid, |snap| {
                            snap.status = DownloadStatus::Failed {
                                error: err.to_string(),
                            };
                            snap.bytes_per_sec = 0.0;
                            snap.bytes_downloaded = final_tp.bytes_downloaded;
                            snap.speed_samples = final_tp.speed_samples;
                        });
                    }
                }
            }
        });

        Ok(job_id)
    }

    /// Streams, SHA-512 verifies, and extracts a GE-Proton release through the Download Manager,
    /// emitting `DownloadUpdated` events and `ProtonInstalled` on success.
    pub async fn start_ge_proton_download(
        self: &Arc<Self>,
        release: GeProtonRelease,
    ) -> CoreResult<String> {
        let job_id = Uuid::new_v4().to_string();
        let seq = self.next_job_seq.fetch_add(1, Ordering::Relaxed);
        let control = DownloadControl::new(release.tarball_size, 1);
        control.set_bandwidth_limit(self.bandwidth_limit.load(Ordering::SeqCst));

        let initial_snapshot = DownloadJobSnapshot {
            id: job_id.clone(),
            name: release.tag_name.clone(),
            kind: DownloadKind::Proton {
                tag_name: release.tag_name.clone(),
            },
            status: DownloadStatus::Queued,
            bytes_downloaded: 0,
            total_bytes: release.tarball_size,
            bytes_per_sec: 0.0,
            files_extracted: 0,
            total_files: 1,
            current_file: Some(release.tarball_name.clone()),
            speed_samples: Vec::new(),
        };

        {
            let mut map = self.downloads.write().unwrap();
            map.insert(
                job_id.clone(),
                ActiveDownloadEntry {
                    seq,
                    snapshot: initial_snapshot.clone(),
                    control: control.clone(),
                },
            );
        }
        self.emit(LauncherEvent::DownloadUpdated {
            job: initial_snapshot,
        });

        let core = Arc::clone(self);
        let jid = job_id.clone();
        tokio::spawn(async move {
            if control.is_cancelled() {
                core.finalize_cancelled(&jid);
                return;
            }
            if !control.is_paused() {
                core.update_download_snapshot(&jid, |snap| {
                    snap.status = DownloadStatus::Downloading;
                });
            }

            let started = Instant::now();
            struct TelemetryState {
                last_sample_at: Instant,
                last_emit_at: Instant,
                last_sample_bytes: u64,
            }
            let telemetry = Arc::new(Mutex::new(TelemetryState {
                last_sample_at: started,
                last_emit_at: started.checked_sub(Duration::from_secs(1)).unwrap_or(started),
                last_sample_bytes: 0,
            }));

            let prog_core = Arc::clone(&core);
            let prog_jid = jid.clone();
            let prog_ctrl = control.clone();
            let prog_tel = telemetry.clone();
            let on_progress = move |downloaded: u64, total: u64| {
                let mut tel = prog_tel.lock().unwrap();
                let dt = tel.last_sample_at.elapsed();
                let mut new_sample: Option<(f64, SpeedSample)> = None;
                if dt >= Duration::from_millis(100) || (total > 0 && downloaded >= total) {
                    let delta = downloaded.saturating_sub(tel.last_sample_bytes);
                    let bps = if dt.as_secs_f64() > 0.0 {
                        delta as f64 / dt.as_secs_f64()
                    } else {
                        0.0
                    };
                    new_sample = Some((
                        bps,
                        SpeedSample {
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            bytes_per_sec: bps,
                        },
                    ));
                    tel.last_sample_at = Instant::now();
                    tel.last_sample_bytes = downloaded;
                }

                let should_emit = tel.last_emit_at.elapsed() >= Duration::from_millis(50)
                    || (total > 0 && downloaded >= total);
                if should_emit {
                    tel.last_emit_at = Instant::now();
                }
                drop(tel);

                let is_paused = prog_ctrl.is_paused();
                let maybe_snap = {
                    let mut map = prog_core.downloads.write().unwrap();
                    if let Some(entry) = map.get_mut(&prog_jid) {
                        entry.snapshot.bytes_downloaded = downloaded;
                        if total > 0 {
                            entry.snapshot.total_bytes = total;
                        }
                        if !matches!(
                            entry.snapshot.status,
                            DownloadStatus::Cancelled
                                | DownloadStatus::Completed
                                | DownloadStatus::Failed { .. }
                                | DownloadStatus::Verifying
                        ) {
                            entry.snapshot.status = if is_paused {
                                DownloadStatus::Paused
                            } else {
                                DownloadStatus::Downloading
                            };
                        }
                        if let Some((bps, sample)) = new_sample {
                            entry.snapshot.bytes_per_sec = if is_paused { 0.0 } else { bps };
                            entry.snapshot.speed_samples.push(sample);
                            if entry.snapshot.speed_samples.len() > 60 {
                                entry.snapshot.speed_samples.remove(0);
                            }
                        }
                        if should_emit {
                            Some(entry.snapshot.clone())
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                };
                if let Some(job) = maybe_snap {
                    prog_core.emit(LauncherEvent::DownloadUpdated { job });
                }
            };

            let verify_core = Arc::clone(&core);
            let verify_jid = jid.clone();
            let on_verifying = move || {
                verify_core.update_download_snapshot(&verify_jid, |snap| {
                    if !matches!(snap.status, DownloadStatus::Cancelled) {
                        snap.status = DownloadStatus::Verifying;
                        snap.bytes_per_sec = 0.0;
                    }
                });
            };

            let res = download_and_install_ge_proton_with_control(
                &release.tarball_url,
                release.sha512_url.as_deref(),
                &core.paths.proton_dir,
                control.paused_flag(),
                control.cancelled_flag(),
                control.bandwidth_limit_flag(),
                on_progress,
                on_verifying,
            )
            .await;

            match res {
                Ok(installation) => {
                    core.update_download_snapshot(&jid, |snap| {
                        snap.status = DownloadStatus::Completed;
                        let final_bytes = snap.bytes_downloaded.max(snap.total_bytes);
                        snap.bytes_downloaded = final_bytes;
                        snap.total_bytes = final_bytes;
                        snap.bytes_per_sec = 0.0;
                        snap.files_extracted = 1;
                        snap.total_files = 1;
                        snap.current_file = None;
                    });
                    core.emit(LauncherEvent::ProtonInstalled { installation });
                }
                Err(ProtonError::Cancelled) => {
                    core.finalize_cancelled(&jid);
                }
                Err(err) => {
                    if control.is_cancelled() {
                        core.finalize_cancelled(&jid);
                    } else {
                        core.update_download_snapshot(&jid, |snap| {
                            snap.status = DownloadStatus::Failed {
                                error: err.to_string(),
                            };
                            snap.bytes_per_sec = 0.0;
                        });
                    }
                }
            }
        });

        Ok(job_id)
    }

    pub async fn pause_download(&self, job_id: &str) -> bool {
        let updated = {
            let mut map = self.downloads.write().unwrap();
            let Some(entry) = map.get_mut(job_id) else {
                return false;
            };
            if !matches!(
                entry.snapshot.status,
                DownloadStatus::Queued | DownloadStatus::Downloading
            ) {
                return false;
            }
            entry.control.pause();
            entry.snapshot.status = DownloadStatus::Paused;
            entry.snapshot.bytes_per_sec = 0.0;
            entry.snapshot.clone()
        };
        self.emit(LauncherEvent::DownloadUpdated { job: updated });
        true
    }

    pub async fn resume_download(&self, job_id: &str) -> bool {
        let updated = {
            let mut map = self.downloads.write().unwrap();
            let Some(entry) = map.get_mut(job_id) else {
                return false;
            };
            if !matches!(entry.snapshot.status, DownloadStatus::Paused) {
                return false;
            }
            entry.control.resume();
            entry.snapshot.status = DownloadStatus::Downloading;
            entry.snapshot.clone()
        };
        self.emit(LauncherEvent::DownloadUpdated { job: updated });
        true
    }

    pub async fn cancel_download(&self, job_id: &str) -> bool {
        let updated = {
            let mut map = self.downloads.write().unwrap();
            let Some(entry) = map.get_mut(job_id) else {
                return false;
            };
            if !matches!(
                entry.snapshot.status,
                DownloadStatus::Queued
                    | DownloadStatus::Downloading
                    | DownloadStatus::Paused
                    | DownloadStatus::Verifying
            ) {
                return false;
            }
            entry.control.cancel();
            entry.snapshot.status = DownloadStatus::Cancelled;
            entry.snapshot.bytes_per_sec = 0.0;
            entry.snapshot.clone()
        };
        self.emit(LauncherEvent::DownloadUpdated { job: updated });
        true
    }

    /// Sets the global download bandwidth limit in bytes/sec (0 = unlimited) and applies it
    /// immediately to all active and future download jobs.
    pub async fn set_bandwidth_limit(&self, bytes_per_sec: u64) {
        self.bandwidth_limit.store(bytes_per_sec, Ordering::SeqCst);
        let map = self.downloads.read().unwrap();
        for entry in map.values() {
            entry.control.set_bandwidth_limit(bytes_per_sec);
        }
    }

    /// Returns a snapshot of all tracked downloads in insertion order.
    pub async fn list_downloads(&self) -> Vec<DownloadJobSnapshot> {
        let map = self.downloads.read().unwrap();
        let mut items: Vec<(u64, DownloadJobSnapshot)> = map
            .values()
            .map(|entry| {
                let mut snap = entry.snapshot.clone();
                if matches!(entry.snapshot.kind, DownloadKind::Game { .. })
                    && matches!(
                        snap.status,
                        DownloadStatus::Downloading | DownloadStatus::Paused
                    )
                {
                    let tp = entry.control.snapshot();
                    snap.bytes_downloaded = tp.bytes_downloaded.max(snap.bytes_downloaded);
                    if tp.total_bytes > 0 {
                        snap.total_bytes = tp.total_bytes;
                    }
                    snap.files_extracted = tp.files_extracted.max(snap.files_extracted);
                    if tp.total_files > 0 {
                        snap.total_files = tp.total_files;
                    }
                    if tp.current_file.is_some() {
                        snap.current_file = tp.current_file;
                    }
                    if !matches!(snap.status, DownloadStatus::Paused) {
                        snap.bytes_per_sec = tp.bytes_per_sec;
                    }
                    if !tp.speed_samples.is_empty() {
                        snap.speed_samples = tp.speed_samples;
                    }
                }
                (entry.seq, snap)
            })
            .collect();
        items.sort_by_key(|(seq, _)| *seq);
        items.into_iter().map(|(_, s)| s).collect()
    }

    /// Uninstalls a downloaded game for the active profile by removing its install directory
    /// from disk and deleting its database record.
    pub async fn uninstall_game(&self, game_id: i64) -> CoreResult<()> {
        let profile = self
            .active_profile()
            .await
            .ok_or(CoreError::NoActiveProfile)?;
        let installed = self
            .db
            .get_installed_game(&profile.id, game_id)?
            .ok_or(CoreError::GameNotInstalled(game_id))?;

        if installed.install_dir.exists() {
            let dir = installed.install_dir.clone();
            tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&dir))
                .await
                .map_err(|e| std::io::Error::other(format!("uninstall task panicked: {e}")))??;
        }
        self.db.remove_installed_game(&profile.id, game_id)?;
        let _ = self.db.clear_pending_sync(&profile.id, game_id);
        Ok(())
    }

    /// Sets or clears the custom relative executable path for an installed game.
    pub async fn set_game_executable(
        &self,
        game_id: i64,
        rel_path: Option<String>,
    ) -> CoreResult<()> {
        let profile = self
            .active_profile()
            .await
            .ok_or(CoreError::NoActiveProfile)?;
        if self.db.get_installed_game(&profile.id, game_id)?.is_none() {
            return Err(CoreError::GameNotInstalled(game_id));
        }
        let cleaned = match rel_path {
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    let safe = safe_rel_path(trimmed)?;
                    Some(safe.to_string_lossy().replace('\\', "/"))
                }
            }
            None => None,
        };
        self.db
            .update_game_executable(&profile.id, game_id, cleaned.as_deref())?;
        Ok(())
    }

    /// Sets or clears the custom Proton runtime override for an installed game.
    pub async fn set_game_proton(
        &self,
        game_id: i64,
        proton_name: Option<String>,
    ) -> CoreResult<()> {
        let profile = self
            .active_profile()
            .await
            .ok_or(CoreError::NoActiveProfile)?;
        if self.db.get_installed_game(&profile.id, game_id)?.is_none() {
            return Err(CoreError::GameNotInstalled(game_id));
        }
        let cleaned = proton_name.and_then(|raw| {
            let trimmed = raw.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        });
        self.db
            .update_game_proton_override(&profile.id, game_id, cleaned.as_deref())?;
        Ok(())
    }

    /// Lists all pending offline save syncs queued for the active profile.
    pub async fn list_pending_syncs(&self) -> CoreResult<Vec<PendingSync>> {
        let profile = self
            .active_profile()
            .await
            .ok_or(CoreError::NoActiveProfile)?;
        Ok(self.db.list_pending_syncs(&profile.id)?)
    }

    /// Runs an on-demand differential save sync for a downloaded game.
    pub async fn sync_game_now(
        &self,
        game_id: i64,
        force_mass_deletion: bool,
    ) -> CoreResult<SyncReport> {
        let (profile, client) = self.active_profile_and_client().await?;
        let installed = self
            .db
            .get_installed_game(&profile.id, game_id)?
            .ok_or(CoreError::GameNotInstalled(game_id))?;

        let report =
            sync_game_saves(&client, game_id, &installed.install_dir, force_mass_deletion).await?;
        self.db
            .update_game_synced_at(&profile.id, game_id, Utc::now())?;
        self.db.clear_pending_sync(&profile.id, game_id)?;
        Ok(report)
    }

    /// Flushes all offline queued save syncs for the active profile.
    pub async fn flush_pending_syncs(&self) -> CoreResult<usize> {
        let (profile, client) = self.active_profile_and_client().await?;
        let pending = self.db.list_pending_syncs(&profile.id)?;
        let mut flushed = 0usize;
        for item in pending {
            if let Some(installed) = self.db.get_installed_game(&profile.id, item.game_id)?
                && sync_game_saves(&client, item.game_id, &installed.install_dir, false)
                    .await
                    .is_ok()
            {
                let _ = self
                    .db
                    .update_game_synced_at(&profile.id, item.game_id, Utc::now());
                let _ = self.db.clear_pending_sync(&profile.id, item.game_id);
                flushed += 1;
            }
        }
        Ok(flushed)
    }

    /// Launches a downloaded game, tracks screen time with suspend protection, and automatically
    /// syncs saves when the game closes (or queues an offline sync if the server is unreachable).
    pub async fn launch_game(
        &self,
        game_id: i64,
        mut options: LaunchOptions,
    ) -> CoreResult<SessionOutcome> {
        let (profile, client, connected) = {
            let guard = self.active.read().await;
            let active = guard.as_ref().ok_or(CoreError::NoActiveProfile)?;
            (
                active.profile.clone(),
                active.client.clone(),
                active.state == ConnectionState::Connected,
            )
        };
        let installed = self
            .db
            .get_installed_game(&profile.id, game_id)?
            .ok_or(CoreError::GameNotInstalled(game_id))?;

        options.prefixes_root = self.paths.prefixes_dir.clone();
        let proton_list = self.detect_proton();
        let chosen_proton = installed
            .proton_override
            .as_deref()
            .and_then(|name| proton_list.iter().find(|p| p.name == name))
            .or_else(|| proton_list.first());

        self.emit(LauncherEvent::GameSessionStarted {
            game_id,
            name: installed.name.clone(),
        });

        let sync_client = if connected { Some(&client) } else { None };
        match launch_and_track(&self.db, sync_client, &installed, chosen_proton, options).await {
            Ok(outcome) => {
                let error = match &outcome.sync_status {
                    PostExitSyncStatus::QueuedOffline(msg) => Some(msg.clone()),
                    PostExitSyncStatus::Synced(_) | PostExitSyncStatus::SkippedDisabled => None,
                };
                self.emit(LauncherEvent::GameSessionEnded {
                    game_id,
                    duration_secs: outcome.session.duration_secs,
                    synced: outcome.session.synced,
                    error,
                });
                Ok(outcome)
            }
            Err(err) => {
                self.emit(LauncherEvent::GameSessionEnded {
                    game_id,
                    duration_secs: 0,
                    synced: false,
                    error: Some(err.to_string()),
                });
                Err(err.into())
            }
        }
    }

    pub async fn playtime_summary(&self, game_id: i64) -> CoreResult<PlaytimeSummary> {
        let profile = self
            .active_profile()
            .await
            .ok_or(CoreError::NoActiveProfile)?;
        Ok(self.db.playtime_summary(&profile.id, game_id)?)
    }

    pub fn detect_proton(&self) -> Vec<ProtonInstallation> {
        let roots = default_proton_search_roots(&self.paths.proton_dir);
        detect_proton_in_roots(&roots)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_installed_game(
        &self,
        profile_id: &str,
        game_id: i64,
        name: &str,
        slug: &str,
        install_dir: &Path,
        outcome: &DownloadOutcome,
        synced_overlay: bool,
    ) -> CoreResult<InstalledGame> {
        let discovered = discover_executables(install_dir, name);
        let executable_rel_path = discovered.into_iter().next().map(|e| e.rel_path);

        let installed = InstalledGame {
            profile_id: profile_id.to_string(),
            game_id,
            name: name.to_string(),
            slug: slug.to_string(),
            install_dir: outcome.install_dir.clone(),
            executable_rel_path,
            manifest_hash: Some(outcome.manifest_hash.clone()),
            total_size: outcome.total_bytes,
            installed_at: Utc::now(),
            last_synced_at: if synced_overlay {
                Some(Utc::now())
            } else {
                None
            },
            proton_override: None,
        };
        self.db.upsert_installed_game(&installed)?;
        Ok(installed)
    }

    fn update_download_snapshot<F>(&self, job_id: &str, mutate: F)
    where
        F: FnOnce(&mut DownloadJobSnapshot),
    {
        let updated = {
            let mut map = self.downloads.write().unwrap();
            let Some(entry) = map.get_mut(job_id) else {
                return;
            };
            mutate(&mut entry.snapshot);
            entry.snapshot.clone()
        };
        self.emit(LauncherEvent::DownloadUpdated { job: updated });
    }

    fn finalize_cancelled(&self, job_id: &str) {
        let maybe_updated = {
            let mut map = self.downloads.write().unwrap();
            let Some(entry) = map.get_mut(job_id) else {
                return;
            };
            if matches!(entry.snapshot.status, DownloadStatus::Cancelled) {
                None
            } else {
                entry.snapshot.status = DownloadStatus::Cancelled;
                entry.snapshot.bytes_per_sec = 0.0;
                Some(entry.snapshot.clone())
            }
        };
        if let Some(job) = maybe_updated {
            self.emit(LauncherEvent::DownloadUpdated { job });
        }
    }

    async fn active_client(&self) -> CoreResult<OpenSyncClient> {
        let guard = self.active.read().await;
        let active = guard.as_ref().ok_or(CoreError::NoActiveProfile)?;
        Ok(active.client.clone())
    }

    async fn active_profile_and_client(&self) -> CoreResult<(Profile, OpenSyncClient)> {
        let guard = self.active.read().await;
        let active = guard.as_ref().ok_or(CoreError::NoActiveProfile)?;
        Ok((active.profile.clone(), active.client.clone()))
    }
}

fn ext_for_mime(content_type: &str) -> &'static str {
    match content_type.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}
