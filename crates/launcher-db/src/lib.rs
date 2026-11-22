use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("profile not found: {0}")]
    ProfileNotFound(String),
}

pub type DbResult<T> = Result<T, DbError>;

/// Default directory for launcher state (SQLite database, `secrets.json`, caches, Proton
/// installs and prefixes).
///
/// Resolution order:
/// 1. `OPENSYNC_LAUNCHER_DATA` override (any platform)
/// 2. macOS: `~/Library/Application Support/opensync-launcher`
/// 3. Linux/BSD: `$XDG_DATA_HOME/opensync-launcher` or `~/.local/share/opensync-launcher`
/// 4. Windows: `%LOCALAPPDATA%/opensync-launcher`, `%APPDATA%/opensync-launcher`, or `USERPROFILE/AppData/Local/...`
///
/// macOS never relies on Linux's `$XDG_DATA_HOME` convention.
pub fn default_data_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("OPENSYNC_LAUNCHER_DATA")
        && !custom.trim().is_empty()
    {
        return PathBuf::from(custom);
    }

    let home = ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(|var| std::env::var(var).ok())
        .find(|v| !v.trim().is_empty())
        .map(PathBuf::from);

    #[cfg(target_os = "macos")]
    if let Some(home) = home {
        return home.join("Library/Application Support/opensync-launcher");
    }

    #[cfg(target_os = "windows")]
    {
        for var in ["LOCALAPPDATA", "APPDATA"] {
            if let Ok(val) = std::env::var(var)
                && !val.trim().is_empty()
            {
                return PathBuf::from(val).join("opensync-launcher");
            }
        }
        if let Some(home) = home {
            return home.join("AppData/Local/opensync-launcher");
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
            && !xdg.trim().is_empty()
        {
            return PathBuf::from(xdg).join("opensync-launcher");
        }
        if let Some(home) = home {
            return home.join(".local/share/opensync-launcher");
        }
    }

    PathBuf::from("./opensync-launcher-data")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub label: String,
    pub server_url: String,
    pub username: String,
    pub user_id: i64,
    pub role: String,
    pub created_at: DateTime<Utc>,
    pub last_connected_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledGame {
    pub profile_id: String,
    pub game_id: i64,
    pub name: String,
    pub slug: String,
    pub install_dir: PathBuf,
    pub executable_rel_path: Option<String>,
    pub manifest_hash: Option<String>,
    pub total_size: u64,
    pub installed_at: DateTime<Utc>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub proton_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaySession {
    pub id: i64,
    pub profile_id: String,
    pub game_id: i64,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_secs: u64,
    pub runtime: String,
    pub synced: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaytimeSummary {
    pub total_secs: u64,
    pub session_count: u64,
    pub last_played_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingSync {
    pub id: i64,
    pub profile_id: String,
    pub game_id: i64,
    pub queued_at: DateTime<Utc>,
    pub reason: String,
}

/// Live game session persisted in SQLite while a game is running so a force-quit or crash
/// can be detected on the next launcher startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveSessionRecord {
    pub profile_id: String,
    pub game_id: i64,
    pub game_name: String,
    pub install_dir: PathBuf,
    pub runtime: String,
    pub pid: Option<u32>,
    pub started_at: DateTime<Utc>,
    pub last_heartbeat_at: DateTime<Utc>,
    pub active_secs: u64,
}

/// Persistent activity log entry for game launches, post-close auto-syncs, manual syncs,
/// force-quit recovery detections, and safe shutdown events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityLogEntry {
    pub id: i64,
    pub profile_id: String,
    pub game_id: Option<i64>,
    pub game_name: Option<String>,
    pub kind: String,
    pub level: String,
    pub title: String,
    pub detail: String,
    pub created_at: DateTime<Utc>,
}

/// Pluggable secret storage for session cookies/tokens.
/// Production uses OS keyring or 0600-restricted secret file; tests use `MemorySecretStore`.
pub trait SecretStore: Send + Sync {
    fn set(&self, profile_id: &str, cookie: &str) -> DbResult<()>;
    fn get(&self, profile_id: &str) -> DbResult<Option<String>>;
    fn delete(&self, profile_id: &str) -> DbResult<()>;
}

#[derive(Default)]
pub struct MemorySecretStore {
    inner: Mutex<HashMap<String, String>>,
}

impl MemorySecretStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl SecretStore for MemorySecretStore {
    fn set(&self, profile_id: &str, cookie: &str) -> DbResult<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(profile_id.to_string(), cookie.to_string());
        Ok(())
    }

    fn get(&self, profile_id: &str) -> DbResult<Option<String>> {
        Ok(self.inner.lock().unwrap().get(profile_id).cloned())
    }

    fn delete(&self, profile_id: &str) -> DbResult<()> {
        self.inner.lock().unwrap().remove(profile_id);
        Ok(())
    }
}

/// File-backed secret store with 0600 permissions on Unix for standalone persistence across restarts.
pub struct FileSecretStore {
    path: PathBuf,
    lock: Mutex<()>,
}

impl FileSecretStore {
    pub fn new(path: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self {
            path: path.into(),
            lock: Mutex::new(()),
        })
    }

    fn read_map(&self) -> DbResult<HashMap<String, String>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let raw = std::fs::read_to_string(&self.path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn write_map(&self, map: &HashMap<String, String>) -> DbResult<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(map)?;
        std::fs::write(&self.path, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

impl SecretStore for FileSecretStore {
    fn set(&self, profile_id: &str, cookie: &str) -> DbResult<()> {
        let _guard = self.lock.lock().unwrap();
        let mut map = self.read_map()?;
        map.insert(profile_id.to_string(), cookie.to_string());
        self.write_map(&map)
    }

    fn get(&self, profile_id: &str) -> DbResult<Option<String>> {
        let _guard = self.lock.lock().unwrap();
        Ok(self.read_map()?.remove(profile_id))
    }

    fn delete(&self, profile_id: &str) -> DbResult<()> {
        let _guard = self.lock.lock().unwrap();
        let mut map = self.read_map()?;
        map.remove(profile_id);
        self.write_map(&map)
    }
}

#[derive(Clone)]
pub struct LauncherDb {
    conn: Arc<Mutex<Connection>>,
}

impl LauncherDb {
    pub fn open(path: impl AsRef<Path>) -> DbResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_schema()?;
        Ok(db)
    }

    pub fn open_in_memory() -> DbResult<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.init_schema()?;
        Ok(db)
    }

    fn init_schema(&self) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS app_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS profiles (
                id TEXT PRIMARY KEY,
                label TEXT NOT NULL,
                server_url TEXT NOT NULL,
                username TEXT NOT NULL,
                user_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_connected_at TEXT
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_profiles_server_user
                ON profiles(server_url, lower(username));

            CREATE TABLE IF NOT EXISTS installed_games (
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                game_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                slug TEXT NOT NULL,
                install_dir TEXT NOT NULL,
                executable_rel_path TEXT,
                manifest_hash TEXT,
                total_size INTEGER NOT NULL DEFAULT 0,
                installed_at TEXT NOT NULL,
                last_synced_at TEXT,
                proton_override TEXT,
                PRIMARY KEY (profile_id, game_id)
            );

            CREATE TABLE IF NOT EXISTS remembered_install_dirs (
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                game_id INTEGER NOT NULL,
                install_dir TEXT NOT NULL,
                PRIMARY KEY (profile_id, game_id)
            );

            CREATE TABLE IF NOT EXISTS play_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                game_id INTEGER NOT NULL,
                started_at TEXT NOT NULL,
                ended_at TEXT NOT NULL,
                duration_secs INTEGER NOT NULL,
                runtime TEXT NOT NULL,
                synced INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_sessions_profile_game
                ON play_sessions(profile_id, game_id);

            CREATE TABLE IF NOT EXISTS pending_syncs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                game_id INTEGER NOT NULL,
                queued_at TEXT NOT NULL,
                reason TEXT NOT NULL,
                UNIQUE(profile_id, game_id)
            );

            CREATE TABLE IF NOT EXISTS active_sessions (
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                game_id INTEGER NOT NULL,
                game_name TEXT NOT NULL,
                install_dir TEXT NOT NULL,
                runtime TEXT NOT NULL DEFAULT 'native',
                pid INTEGER,
                started_at TEXT NOT NULL,
                last_heartbeat_at TEXT NOT NULL,
                active_secs INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (profile_id, game_id)
            );

            CREATE TABLE IF NOT EXISTS activity_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_id TEXT NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
                game_id INTEGER,
                game_name TEXT,
                kind TEXT NOT NULL,
                level TEXT NOT NULL DEFAULT 'info',
                title TEXT NOT NULL,
                detail TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_activity_logs_profile_created
                ON activity_logs(profile_id, id DESC);
            "#,
        )?;
        Ok(())
    }

    pub fn upsert_profile(
        &self,
        label: &str,
        server_url: &str,
        username: &str,
        user_id: i64,
        role: &str,
    ) -> DbResult<Profile> {
        let normalized_url = server_url.trim_end_matches('/').to_string();
        let now = Utc::now();
        let conn = self.conn.lock().unwrap();

        let existing_id: Option<String> = conn
            .query_row(
                "SELECT id FROM profiles WHERE server_url = ?1 AND lower(username) = lower(?2)",
                params![normalized_url, username],
                |r| r.get(0),
            )
            .optional()?;

        let id = existing_id.unwrap_or_else(|| Uuid::new_v4().to_string());
        conn.execute(
            r#"
            INSERT INTO profiles (id, label, server_url, username, user_id, role, created_at, last_connected_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(id) DO UPDATE SET
                label = excluded.label,
                username = excluded.username,
                user_id = excluded.user_id,
                role = excluded.role,
                last_connected_at = excluded.last_connected_at
            "#,
            params![
                id,
                label,
                normalized_url,
                username,
                user_id,
                role,
                now.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )?;
        drop(conn);
        self.get_profile(&id)?
            .ok_or_else(|| DbError::ProfileNotFound(id))
    }

    pub fn get_profile(&self, id: &str) -> DbResult<Option<Profile>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, label, server_url, username, user_id, role, created_at, last_connected_at FROM profiles WHERE id = ?1",
            params![id],
            Self::row_to_profile,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Renames a saved instance's local label (display name only; server URL and account
    /// are untouched). Returns the updated profile.
    pub fn rename_profile(&self, id: &str, label: &str) -> DbResult<Profile> {
        let label = label.trim();
        if label.is_empty() {
            return Err(DbError::ProfileNotFound(id.to_string()));
        }
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE profiles SET label = ?1 WHERE id = ?2",
            params![label, id],
        )?;
        if changed == 0 {
            return Err(DbError::ProfileNotFound(id.to_string()));
        }
        drop(conn);
        self.get_profile(id)?
            .ok_or_else(|| DbError::ProfileNotFound(id.to_string()))
    }

    /// Updates the account username persisted for a saved profile. The server is the source
    /// of truth for the value; this keeps the local copy (and its unique index) in sync.
    /// A conflicting rename may fail with `DbError::Sqlite` from the unique index.
    pub fn update_profile_username(&self, id: &str, username: &str) -> DbResult<Profile> {
        let username = username.trim();
        if username.is_empty() {
            return Err(DbError::ProfileNotFound(id.to_string()));
        }
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE profiles SET username = ?1 WHERE id = ?2",
            params![username, id],
        )?;
        if changed == 0 {
            return Err(DbError::ProfileNotFound(id.to_string()));
        }
        drop(conn);
        self.get_profile(id)?
            .ok_or_else(|| DbError::ProfileNotFound(id.to_string()))
    }

    pub fn list_profiles(&self) -> DbResult<Vec<Profile>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, label, server_url, username, user_id, role, created_at, last_connected_at FROM profiles ORDER BY COALESCE(last_connected_at, created_at) DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_profile)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn touch_profile_connected(&self, id: &str) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE profiles SET last_connected_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    pub fn delete_profile(&self, id: &str) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        // Explicit cleanup keeps the table tidy even when foreign keys are disabled.
        conn.execute(
            "DELETE FROM remembered_install_dirs WHERE profile_id = ?1",
            params![id],
        )?;
        conn.execute("DELETE FROM profiles WHERE id = ?1", params![id])?;
        conn.execute(
            "DELETE FROM app_state WHERE key = 'active_profile_id' AND value = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn set_active_profile_id(&self, id: Option<&str>) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        match id {
            Some(pid) => {
                conn.execute(
                    "INSERT INTO app_state (key, value) VALUES ('active_profile_id', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![pid],
                )?;
            }
            None => {
                conn.execute("DELETE FROM app_state WHERE key = 'active_profile_id'", [])?;
            }
        }
        Ok(())
    }

    pub fn get_active_profile_id(&self) -> DbResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM app_state WHERE key = 'active_profile_id'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_setting(&self, key: &str) -> DbResult<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM app_state WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO app_state (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn upsert_installed_game(&self, game: &InstalledGame) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO installed_games (
                profile_id, game_id, name, slug, install_dir, executable_rel_path,
                manifest_hash, total_size, installed_at, last_synced_at, proton_override
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(profile_id, game_id) DO UPDATE SET
                name = excluded.name,
                slug = excluded.slug,
                install_dir = excluded.install_dir,
                executable_rel_path = COALESCE(excluded.executable_rel_path, installed_games.executable_rel_path),
                manifest_hash = excluded.manifest_hash,
                total_size = excluded.total_size,
                last_synced_at = COALESCE(excluded.last_synced_at, installed_games.last_synced_at),
                proton_override = COALESCE(excluded.proton_override, installed_games.proton_override)
            "#,
            params![
                game.profile_id,
                game.game_id,
                game.name,
                game.slug,
                game.install_dir.to_string_lossy().to_string(),
                game.executable_rel_path,
                game.manifest_hash,
                game.total_size as i64,
                game.installed_at.to_rfc3339(),
                game.last_synced_at.map(|d| d.to_rfc3339()),
                game.proton_override,
            ],
        )?;
        Ok(())
    }

    pub fn get_installed_game(
        &self,
        profile_id: &str,
        game_id: i64,
    ) -> DbResult<Option<InstalledGame>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            r#"
            SELECT profile_id, game_id, name, slug, install_dir, executable_rel_path,
                   manifest_hash, total_size, installed_at, last_synced_at, proton_override
            FROM installed_games WHERE profile_id = ?1 AND game_id = ?2
            "#,
            params![profile_id, game_id],
            Self::row_to_installed,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_installed_games(&self, profile_id: &str) -> DbResult<Vec<InstalledGame>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT profile_id, game_id, name, slug, install_dir, executable_rel_path,
                   manifest_hash, total_size, installed_at, last_synced_at, proton_override
            FROM installed_games WHERE profile_id = ?1 ORDER BY installed_at DESC
            "#,
        )?;
        let rows = stmt.query_map(params![profile_id], Self::row_to_installed)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn update_game_synced_at(
        &self,
        profile_id: &str,
        game_id: i64,
        synced_at: DateTime<Utc>,
    ) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE installed_games SET last_synced_at = ?1 WHERE profile_id = ?2 AND game_id = ?3",
            params![synced_at.to_rfc3339(), profile_id, game_id],
        )?;
        Ok(())
    }

    pub fn update_game_executable(
        &self,
        profile_id: &str,
        game_id: i64,
        exe_rel_path: Option<&str>,
    ) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE installed_games SET executable_rel_path = ?1 WHERE profile_id = ?2 AND game_id = ?3",
            params![exe_rel_path, profile_id, game_id],
        )?;
        Ok(())
    }

    pub fn update_game_proton_override(
        &self,
        profile_id: &str,
        game_id: i64,
        proton_name: Option<&str>,
    ) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE installed_games SET proton_override = ?1 WHERE profile_id = ?2 AND game_id = ?3",
            params![proton_name, profile_id, game_id],
        )?;
        Ok(())
    }

    pub fn remove_installed_game(&self, profile_id: &str, game_id: i64) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM installed_games WHERE profile_id = ?1 AND game_id = ?2",
            params![profile_id, game_id],
        )?;
        Ok(())
    }

    /// Persists the directory a game was installed into so a later redownload can restore
    /// the exact same path (and its cloud saves) instead of rebuilding from the game slug.
    pub fn set_remembered_install_dir(
        &self,
        profile_id: &str,
        game_id: i64,
        install_dir: &Path,
    ) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO remembered_install_dirs (profile_id, game_id, install_dir)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(profile_id, game_id) DO UPDATE SET install_dir = excluded.install_dir
            "#,
            params![
                profile_id,
                game_id,
                install_dir.to_string_lossy().to_string()
            ],
        )?;
        Ok(())
    }

    pub fn get_remembered_install_dir(
        &self,
        profile_id: &str,
        game_id: i64,
    ) -> DbResult<Option<PathBuf>> {
        let conn = self.conn.lock().unwrap();
        let raw: Option<String> = conn
            .query_row(
                "SELECT install_dir FROM remembered_install_dirs WHERE profile_id = ?1 AND game_id = ?2",
                params![profile_id, game_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.map(PathBuf::from))
    }

    pub fn clear_remembered_install_dir(&self, profile_id: &str, game_id: i64) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM remembered_install_dirs WHERE profile_id = ?1 AND game_id = ?2",
            params![profile_id, game_id],
        )?;
        Ok(())
    }

    pub fn record_play_session(
        &self,
        profile_id: &str,
        game_id: i64,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
        duration_secs: u64,
        runtime: &str,
        synced: bool,
    ) -> DbResult<PlaySession> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO play_sessions (profile_id, game_id, started_at, ended_at, duration_secs, runtime, synced)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                profile_id,
                game_id,
                started_at.to_rfc3339(),
                ended_at.to_rfc3339(),
                duration_secs as i64,
                runtime,
                if synced { 1 } else { 0 },
            ],
        )?;
        let id = conn.last_insert_rowid();
        Ok(PlaySession {
            id,
            profile_id: profile_id.to_string(),
            game_id,
            started_at,
            ended_at,
            duration_secs,
            runtime: runtime.to_string(),
            synced,
        })
    }

    pub fn mark_session_synced(&self, session_id: i64) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE play_sessions SET synced = 1 WHERE id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    pub fn list_play_sessions(&self, profile_id: &str, game_id: i64) -> DbResult<Vec<PlaySession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, profile_id, game_id, started_at, ended_at, duration_secs, runtime, synced
            FROM play_sessions WHERE profile_id = ?1 AND game_id = ?2 ORDER BY started_at DESC
            "#,
        )?;
        let rows = stmt.query_map(params![profile_id, game_id], |r| {
            let started: String = r.get(3)?;
            let ended: String = r.get(4)?;
            let synced_int: i64 = r.get(7)?;
            Ok(PlaySession {
                id: r.get(0)?,
                profile_id: r.get(1)?,
                game_id: r.get(2)?,
                started_at: parse_dt(&started),
                ended_at: parse_dt(&ended),
                duration_secs: r.get::<_, i64>(5)?.max(0) as u64,
                runtime: r.get(6)?,
                synced: synced_int != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn playtime_summary(&self, profile_id: &str, game_id: i64) -> DbResult<PlaytimeSummary> {
        let conn = self.conn.lock().unwrap();
        let (total, count, last): (i64, i64, Option<String>) = conn.query_row(
            "SELECT COALESCE(SUM(duration_secs), 0), COUNT(*), MAX(ended_at) FROM play_sessions WHERE profile_id = ?1 AND game_id = ?2",
            params![profile_id, game_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        Ok(PlaytimeSummary {
            total_secs: total.max(0) as u64,
            session_count: count.max(0) as u64,
            last_played_at: last.as_deref().map(parse_dt),
        })
    }

    pub fn playtime_summaries_for_profile(
        &self,
        profile_id: &str,
    ) -> DbResult<HashMap<i64, PlaytimeSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT game_id, COALESCE(SUM(duration_secs), 0), COUNT(*), MAX(ended_at) FROM play_sessions WHERE profile_id = ?1 GROUP BY game_id",
        )?;
        let rows = stmt.query_map(params![profile_id], |r| {
            let gid: i64 = r.get(0)?;
            let total: i64 = r.get(1)?;
            let count: i64 = r.get(2)?;
            let last: Option<String> = r.get(3)?;
            Ok((
                gid,
                PlaytimeSummary {
                    total_secs: total.max(0) as u64,
                    session_count: count.max(0) as u64,
                    last_played_at: last.as_deref().map(parse_dt),
                },
            ))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (gid, summary) = row?;
            map.insert(gid, summary);
        }
        Ok(map)
    }

    pub fn queue_pending_sync(
        &self,
        profile_id: &str,
        game_id: i64,
        reason: &str,
    ) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO pending_syncs (profile_id, game_id, queued_at, reason)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(profile_id, game_id) DO UPDATE SET
                queued_at = excluded.queued_at,
                reason = excluded.reason
            "#,
            params![profile_id, game_id, Utc::now().to_rfc3339(), reason],
        )?;
        Ok(())
    }

    pub fn list_pending_syncs(&self, profile_id: &str) -> DbResult<Vec<PendingSync>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, profile_id, game_id, queued_at, reason FROM pending_syncs WHERE profile_id = ?1 ORDER BY queued_at ASC",
        )?;
        let rows = stmt.query_map(params![profile_id], |r| {
            let queued: String = r.get(3)?;
            Ok(PendingSync {
                id: r.get(0)?,
                profile_id: r.get(1)?,
                game_id: r.get(2)?,
                queued_at: parse_dt(&queued),
                reason: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn clear_pending_sync(&self, profile_id: &str, game_id: i64) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM pending_syncs WHERE profile_id = ?1 AND game_id = ?2",
            params![profile_id, game_id],
        )?;
        Ok(())
    }

    pub fn upsert_active_session(&self, rec: &ActiveSessionRecord) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO active_sessions (
                profile_id, game_id, game_name, install_dir, runtime, pid,
                started_at, last_heartbeat_at, active_secs
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(profile_id, game_id) DO UPDATE SET
                game_name = excluded.game_name,
                install_dir = excluded.install_dir,
                runtime = excluded.runtime,
                pid = excluded.pid,
                started_at = excluded.started_at,
                last_heartbeat_at = excluded.last_heartbeat_at,
                active_secs = excluded.active_secs
            "#,
            params![
                rec.profile_id,
                rec.game_id,
                rec.game_name,
                rec.install_dir.to_string_lossy().to_string(),
                rec.runtime,
                rec.pid.map(|p| p as i64),
                rec.started_at.to_rfc3339(),
                rec.last_heartbeat_at.to_rfc3339(),
                rec.active_secs as i64,
            ],
        )?;
        Ok(())
    }

    pub fn heartbeat_active_session(
        &self,
        profile_id: &str,
        game_id: i64,
        heartbeat_at: DateTime<Utc>,
        active_secs: u64,
    ) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            UPDATE active_sessions
            SET last_heartbeat_at = ?1, active_secs = ?2
            WHERE profile_id = ?3 AND game_id = ?4
            "#,
            params![
                heartbeat_at.to_rfc3339(),
                active_secs as i64,
                profile_id,
                game_id,
            ],
        )?;
        Ok(())
    }

    pub fn get_active_session(
        &self,
        profile_id: &str,
        game_id: i64,
    ) -> DbResult<Option<ActiveSessionRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            r#"
            SELECT profile_id, game_id, game_name, install_dir, runtime, pid,
                   started_at, last_heartbeat_at, active_secs
            FROM active_sessions WHERE profile_id = ?1 AND game_id = ?2
            "#,
            params![profile_id, game_id],
            Self::row_to_active_session,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_active_sessions(&self, profile_id: &str) -> DbResult<Vec<ActiveSessionRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT profile_id, game_id, game_name, install_dir, runtime, pid,
                   started_at, last_heartbeat_at, active_secs
            FROM active_sessions WHERE profile_id = ?1 ORDER BY started_at DESC
            "#,
        )?;
        let rows = stmt.query_map(params![profile_id], Self::row_to_active_session)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn clear_active_session(&self, profile_id: &str, game_id: i64) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM active_sessions WHERE profile_id = ?1 AND game_id = ?2",
            params![profile_id, game_id],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_activity_log(
        &self,
        profile_id: &str,
        game_id: Option<i64>,
        game_name: Option<&str>,
        kind: &str,
        level: &str,
        title: &str,
        detail: &str,
    ) -> DbResult<ActivityLogEntry> {
        let now = Utc::now();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            r#"
            INSERT INTO activity_logs (profile_id, game_id, game_name, kind, level, title, detail, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                profile_id,
                game_id,
                game_name,
                kind,
                level,
                title,
                detail,
                now.to_rfc3339(),
            ],
        )?;
        let id = conn.last_insert_rowid();
        // Keep the latest 200 log entries per profile so the table stays bounded.
        let _ = conn.execute(
            r#"
            DELETE FROM activity_logs
            WHERE profile_id = ?1
              AND id NOT IN (
                  SELECT id FROM activity_logs WHERE profile_id = ?1 ORDER BY id DESC LIMIT 200
              )
            "#,
            params![profile_id],
        );
        Ok(ActivityLogEntry {
            id,
            profile_id: profile_id.to_string(),
            game_id,
            game_name: game_name.map(|s| s.to_string()),
            kind: kind.to_string(),
            level: level.to_string(),
            title: title.to_string(),
            detail: detail.to_string(),
            created_at: now,
        })
    }

    pub fn list_activity_logs(
        &self,
        profile_id: &str,
        limit: usize,
    ) -> DbResult<Vec<ActivityLogEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            r#"
            SELECT id, profile_id, game_id, game_name, kind, level, title, detail, created_at
            FROM activity_logs
            WHERE profile_id = ?1
            ORDER BY id DESC
            LIMIT ?2
            "#,
        )?;
        let rows = stmt.query_map(params![profile_id, limit.max(1) as i64], |r| {
            let created: String = r.get(8)?;
            Ok(ActivityLogEntry {
                id: r.get(0)?,
                profile_id: r.get(1)?,
                game_id: r.get(2)?,
                game_name: r.get(3)?,
                kind: r.get(4)?,
                level: r.get(5)?,
                title: r.get(6)?,
                detail: r.get(7)?,
                created_at: parse_dt(&created),
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn clear_activity_logs(&self, profile_id: &str) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM activity_logs WHERE profile_id = ?1",
            params![profile_id],
        )?;
        Ok(())
    }

    /// Flushes and truncates the SQLite Write-Ahead Log so all committed state is safely
    /// merged into the main database file before application shutdown.
    pub fn checkpoint_wal(&self) -> DbResult<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    fn row_to_active_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<ActiveSessionRecord> {
        let install_dir: String = r.get(3)?;
        let pid_opt: Option<i64> = r.get(5)?;
        let started: String = r.get(6)?;
        let heartbeat: String = r.get(7)?;
        let active_secs: i64 = r.get(8)?;
        Ok(ActiveSessionRecord {
            profile_id: r.get(0)?,
            game_id: r.get(1)?,
            game_name: r.get(2)?,
            install_dir: PathBuf::from(install_dir),
            runtime: r.get(4)?,
            pid: pid_opt.and_then(|v| u32::try_from(v).ok()),
            started_at: parse_dt(&started),
            last_heartbeat_at: parse_dt(&heartbeat),
            active_secs: active_secs.max(0) as u64,
        })
    }

    fn row_to_profile(r: &rusqlite::Row<'_>) -> rusqlite::Result<Profile> {
        let created: String = r.get(6)?;
        let last: Option<String> = r.get(7)?;
        Ok(Profile {
            id: r.get(0)?,
            label: r.get(1)?,
            server_url: r.get(2)?,
            username: r.get(3)?,
            user_id: r.get(4)?,
            role: r.get(5)?,
            created_at: parse_dt(&created),
            last_connected_at: last.as_deref().map(parse_dt),
        })
    }

    fn row_to_installed(r: &rusqlite::Row<'_>) -> rusqlite::Result<InstalledGame> {
        let install_dir: String = r.get(4)?;
        let installed_at: String = r.get(8)?;
        let last_synced: Option<String> = r.get(9)?;
        Ok(InstalledGame {
            profile_id: r.get(0)?,
            game_id: r.get(1)?,
            name: r.get(2)?,
            slug: r.get(3)?,
            install_dir: PathBuf::from(install_dir),
            executable_rel_path: r.get(5)?,
            manifest_hash: r.get(6)?,
            total_size: r.get::<_, i64>(7)?.max(0) as u64,
            installed_at: parse_dt(&installed_at),
            last_synced_at: last_synced.as_deref().map(parse_dt),
            proton_override: r.get(10)?,
        })
    }
}

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_default_uses_platform_location() {
        let dir = std::env::var("OPENSYNC_LAUNCHER_DATA").ok();
        if dir.is_some() {
            return; // An explicit override is always respected; nothing else to assert.
        }
        let resolved = default_data_dir();
        assert!(resolved.ends_with("opensync-launcher"), "{resolved:?}");

        #[cfg(target_os = "macos")]
        assert!(
            resolved.to_string_lossy().contains("Library/Application Support"),
            "macOS data dir must not use XDG paths: {resolved:?}"
        );
    }
}
