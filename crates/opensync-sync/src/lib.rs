use bytes::Bytes;
use opensync_api::{ApiError, GameManifest, OpenSyncClient};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Semaphore;
use walkdir::WalkDir;

const HASH_BUF_SIZE: usize = 256 * 1024;
const UPLOAD_CONCURRENCY: usize = 4;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("api error: {0}")]
    Api(#[from] ApiError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("mass deletion guard triggered: {deleted} of {total} files ({pct}%) missing; set force=true to proceed")]
    MassDeletionGuard {
        deleted: usize,
        total: usize,
        pct: usize,
    },
    #[error("install directory does not exist: {0}")]
    MissingInstallDir(PathBuf),
}

pub type SyncResult<T> = Result<T, SyncError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalFile {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    New,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffUpload {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffPlan {
    pub uploads: Vec<DiffUpload>,
    pub deletions: Vec<String>,
    pub unchanged_count: usize,
}

impl DiffPlan {
    pub fn is_empty(&self) -> bool {
        self.uploads.is_empty() && self.deletions.is_empty()
    }

    pub fn upload_bytes(&self) -> u64 {
        self.uploads.iter().map(|u| u.size).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub uploaded_files: usize,
    pub uploaded_bytes: u64,
    pub deleted_paths: usize,
    pub skipped_unchanged: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSaveInspection {
    pub has_changes: bool,
    pub changed_files: usize,
    pub deleted_files: usize,
    pub changed_bytes: u64,
    pub sample_paths: Vec<String>,
}

/// Scans `root` recursively and computes SHA-256 hashes in parallel via `rayon`.
pub fn scan_folder_parallel(root: &Path) -> SyncResult<Vec<LocalFile>> {
    if !root.exists() {
        return Err(SyncError::MissingInstallDir(root.to_path_buf()));
    }

    let entries: Vec<(String, PathBuf, u64)> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let abs = e.path().to_path_buf();
            let rel = abs.strip_prefix(root).ok()?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if rel_str.ends_with(".part") {
                return None;
            }
            let size = e.metadata().ok()?.len();
            Some((rel_str, abs, size))
        })
        .collect();

    let mut hashed: Vec<LocalFile> = entries
        .into_par_iter()
        .map(|(rel_path, abs_path, size)| {
            let sha256 = hash_file_sha256(&abs_path)?;
            Ok(LocalFile {
                rel_path,
                abs_path,
                size,
                sha256,
            })
        })
        .collect::<Result<Vec<_>, std::io::Error>>()?;

    hashed.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(hashed)
}

pub fn hash_file_sha256(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(HASH_BUF_SIZE, file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Computes the differential save plan strictly against the clean manifest (never chained).
pub fn compute_diff(local_files: &[LocalFile], clean_manifest: &GameManifest) -> DiffPlan {
    let clean_map: HashMap<&str, &opensync_api::ManifestEntry> = clean_manifest
        .files
        .iter()
        .map(|f| (f.path.as_str(), f))
        .collect();
    let local_paths: HashSet<&str> = local_files.iter().map(|f| f.rel_path.as_str()).collect();

    let mut uploads = Vec::new();
    let mut unchanged_count = 0usize;

    for lf in local_files {
        match clean_map.get(lf.rel_path.as_str()) {
            None => uploads.push(DiffUpload {
                rel_path: lf.rel_path.clone(),
                abs_path: lf.abs_path.clone(),
                size: lf.size,
                sha256: lf.sha256.clone(),
                kind: ChangeKind::New,
            }),
            Some(clean) if clean.hash != lf.sha256 => uploads.push(DiffUpload {
                rel_path: lf.rel_path.clone(),
                abs_path: lf.abs_path.clone(),
                size: lf.size,
                sha256: lf.sha256.clone(),
                kind: ChangeKind::Modified,
            }),
            Some(_) => unchanged_count += 1,
        }
    }

    let mut deletions: Vec<String> = clean_manifest
        .files
        .iter()
        .filter(|f| !local_paths.contains(f.path.as_str()))
        .map(|f| f.path.clone())
        .collect();
    deletions.sort();

    DiffPlan {
        uploads,
        deletions,
        unchanged_count,
    }
}

/// Scans `install_dir`, diffs against the server's clean manifest, uploads new/changed files
/// concurrently in resumable chunks (4 MB), and commits deletions via `/sync/complete`.
pub async fn sync_game_saves(
    client: &OpenSyncClient,
    game_id: i64,
    install_dir: &Path,
    force_mass_deletion: bool,
) -> SyncResult<SyncReport> {
    let manifest = client.get_manifest(game_id).await?;
    let dir_clone = install_dir.to_path_buf();
    let local_files = tokio::task::spawn_blocking(move || scan_folder_parallel(&dir_clone))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))??;

    let diff = compute_diff(&local_files, &manifest);

    // Client-side mass-deletion guard (mirrors server >50% check)
    if !force_mass_deletion
        && !manifest.files.is_empty()
        && diff.deletions.len() * 2 > manifest.files.len()
    {
        let pct = (diff.deletions.len() * 100) / manifest.files.len();
        return Err(SyncError::MassDeletionGuard {
            deleted: diff.deletions.len(),
            total: manifest.files.len(),
            pct,
        });
    }

    if diff.is_empty() {
        // Still commit empty sync if needed? No-op when completely identical to clean manifest
        // unless caller explicitly wants to clear — return clean zero-upload report.
        return Ok(SyncReport {
            uploaded_files: 0,
            uploaded_bytes: 0,
            deleted_paths: 0,
            skipped_unchanged: diff.unchanged_count,
        });
    }

    let init = client.overlay_init(game_id).await?;
    let chunk_size = init.chunk_size.max(1);
    let resume_map: Arc<HashMap<String, usize>> = Arc::new(
        init.files
            .into_iter()
            .map(|p| (p.path, p.received))
            .collect(),
    );

    let sem = Arc::new(Semaphore::new(UPLOAD_CONCURRENCY));
    let mut handles = Vec::with_capacity(diff.uploads.len());

    for item in diff.uploads.clone() {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let resume_from = *resume_map.get(&item.rel_path).unwrap_or(&0);
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            upload_file_chunked(&client, game_id, &item, chunk_size, resume_from).await
        }));
    }

    for h in handles {
        h.await
            .map_err(|e| std::io::Error::other(e.to_string()))??;
    }

    client
        .sync_complete(game_id, &diff.deletions, force_mass_deletion)
        .await?;

    Ok(SyncReport {
        uploaded_files: diff.uploads.len(),
        uploaded_bytes: diff.upload_bytes(),
        deleted_paths: diff.deletions.len(),
        skipped_unchanged: diff.unchanged_count,
    })
}

async fn upload_file_chunked(
    client: &OpenSyncClient,
    game_id: i64,
    item: &DiffUpload,
    chunk_size: usize,
    mut resume_from: usize,
) -> SyncResult<()> {
    let total_chunks = ((item.size as usize).div_ceil(chunk_size)).max(1);
    let mut i = resume_from.min(total_chunks);
    let mut retried_resume = false;

    while i < total_chunks {
        let offset = (i as u64) * (chunk_size as u64);
        let len = ((item.size.saturating_sub(offset)) as usize).min(chunk_size);
        let path = item.abs_path.clone();
        let chunk_bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Bytes> {
            if len == 0 {
                return Ok(Bytes::new());
            }
            let mut file = File::open(&path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; len];
            file.read_exact(&mut buf)?;
            Ok(Bytes::from(buf))
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))??;

        match client
            .upload_overlay_chunk(
                game_id,
                &item.rel_path,
                i,
                total_chunks,
                item.size,
                &item.sha256,
                chunk_bytes,
            )
            .await
        {
            Ok(()) => {
                i += 1;
            }
            Err(ApiError::ResumeRequired { received }) if !retried_resume => {
                retried_resume = true;
                resume_from = received.min(total_chunks);
                i = resume_from;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}

/// Inspects a local game installation directory to determine whether there are unsynced save
/// changes (used when recovering from an interrupted / force-quit game session).
///
/// - When `clean_manifest` is available (online), computes the exact SHA-256 diff against the
///   clean manifest. If `last_synced_at` is set, filters to files modified since `last_synced_at`
///   (with a 1-second grace window for filesystem mtime granularity) or any deletions.
/// - When `clean_manifest` is `None` (offline), falls back to checking whether any file in
///   `install_dir` has an mtime newer than `baseline_time`.
pub fn inspect_unsynced_saves(
    install_dir: &Path,
    clean_manifest: Option<&GameManifest>,
    last_synced_at: Option<std::time::SystemTime>,
    session_started_at: Option<std::time::SystemTime>,
) -> SyncResult<LocalSaveInspection> {
    if !install_dir.exists() {
        return Ok(LocalSaveInspection::default());
    }

    if let Some(manifest) = clean_manifest {
        let local_files = scan_folder_parallel(install_dir)?;
        let diff = compute_diff(&local_files, manifest);
        if diff.is_empty() {
            return Ok(LocalSaveInspection::default());
        }

        let uploads: Vec<&DiffUpload> = if let Some(last_sync) = last_synced_at {
            let cutoff = last_sync
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or(last_sync);
            let recent: Vec<&DiffUpload> = diff
                .uploads
                .iter()
                .filter(|u| {
                    std::fs::metadata(&u.abs_path)
                        .and_then(|m| m.modified())
                        .map(|mtime| mtime >= cutoff)
                        .unwrap_or(true)
                })
                .collect();
            recent
        } else {
            diff.uploads.iter().collect()
        };

        let changed_files = uploads.len();
        let deleted_files = diff.deletions.len();
        let changed_bytes: u64 = uploads.iter().map(|u| u.size).sum();
        let has_changes = changed_files > 0 || deleted_files > 0;
        let mut sample_paths: Vec<String> = uploads
            .iter()
            .take(5)
            .map(|u| u.rel_path.clone())
            .collect();
        if sample_paths.len() < 5 {
            for del in diff.deletions.iter().take(5 - sample_paths.len()) {
                sample_paths.push(format!("{del} (deleted)"));
            }
        }
        return Ok(LocalSaveInspection {
            has_changes,
            changed_files,
            deleted_files,
            changed_bytes,
            sample_paths,
        });
    }

    // Offline fallback: compare file modification times against `last_synced_at` or `session_started_at`.
    let baseline = last_synced_at.or(session_started_at);
    let Some(base) = baseline else {
        return Ok(LocalSaveInspection::default());
    };
    let cutoff = base
        .checked_sub(std::time::Duration::from_secs(1))
        .unwrap_or(base);

    let mut changed_files = 0usize;
    let mut changed_bytes = 0u64;
    let mut sample_paths = Vec::new();

    for entry in WalkDir::new(install_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
    {
        let abs = entry.path();
        let Ok(rel) = abs.strip_prefix(install_dir) else {
            continue;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.ends_with(".part") {
            continue;
        }
        if let Ok(meta) = entry.metadata()
            && let Ok(mtime) = meta.modified()
            && mtime >= cutoff
        {
            changed_files += 1;
            changed_bytes += meta.len();
            if sample_paths.len() < 5 {
                sample_paths.push(rel_str);
            }
        }
    }

    Ok(LocalSaveInspection {
        has_changes: changed_files > 0,
        changed_files,
        deleted_files: 0,
        changed_bytes,
        sample_paths,
    })
}
