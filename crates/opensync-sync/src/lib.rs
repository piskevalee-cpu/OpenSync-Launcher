use bytes::Bytes;
use opensync_api::{ApiError, GameManifest, OpenSyncClient};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc};
use walkdir::WalkDir;

const HASH_BUF_SIZE: usize = 512 * 1024;
const UPLOAD_CONCURRENCY: usize = 6;

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
        .map_init(
            || vec![0u8; HASH_BUF_SIZE],
            |buf, (rel_path, abs_path, size)| {
                let sha256 = hash_file_sha256_with_buf(&abs_path, buf)?;
                Ok(LocalFile {
                    rel_path,
                    abs_path,
                    size,
                    sha256,
                })
            },
        )
        .collect::<Result<Vec<_>, std::io::Error>>()?;

    hashed.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(hashed)
}

fn hash_file_sha256_with_buf(path: &Path, buf: &mut [u8]) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    loop {
        match file.read(buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn hash_file_sha256(path: &Path) -> std::io::Result<String> {
    let mut buf = vec![0u8; HASH_BUF_SIZE];
    hash_file_sha256_with_buf(path, &mut buf)
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
    resume_from: usize,
) -> SyncResult<()> {
    let total_chunks = if item.size == 0 {
        1
    } else {
        item.size.div_ceil(chunk_size as u64) as usize
    };
    let mut start = resume_from.min(total_chunks);
    let mut retried_resume = false;

    while start < total_chunks {
        let path = item.abs_path.clone();
        let size = item.size;
        let (tx, mut rx) = mpsc::channel::<(usize, Bytes)>(2);

        // Single open + sequential read-ahead on blocking pool overlaps disk I/O with HTTP POSTs.
        let reader = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let mut file = File::open(&path)?;
            let start_offset = (start as u64) * (chunk_size as u64);
            if start_offset > 0 {
                file.seek(SeekFrom::Start(start_offset))?;
            }
            for index in start..total_chunks {
                let offset = (index as u64) * (chunk_size as u64);
                let len = ((size.saturating_sub(offset)) as usize).min(chunk_size);
                let data = if len == 0 {
                    Bytes::new()
                } else {
                    let mut buf = vec![0u8; len];
                    file.read_exact(&mut buf)?;
                    Bytes::from(buf)
                };
                if tx.blocking_send((index, data)).is_err() {
                    break;
                }
            }
            Ok(())
        });

        let mut next_resume: Option<usize> = None;
        let mut upload_err: Option<SyncError> = None;

        while let Some((index, chunk_bytes)) = rx.recv().await {
            match client
                .upload_overlay_chunk(
                    game_id,
                    &item.rel_path,
                    index,
                    total_chunks,
                    item.size,
                    &item.sha256,
                    chunk_bytes,
                )
                .await
            {
                Ok(()) => {}
                Err(ApiError::ResumeRequired { received }) if !retried_resume => {
                    retried_resume = true;
                    next_resume = Some(received.min(total_chunks));
                    break;
                }
                Err(e) => {
                    upload_err = Some(e.into());
                    break;
                }
            }
        }

        drop(rx);
        let reader_res = reader
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;

        if let Some(err) = upload_err {
            return Err(err);
        }
        if let Some(new_start) = next_resume {
            start = new_start;
            continue;
        }
        reader_res?;
        break;
    }

    Ok(())
}

/// Inspects a local game installation directory to determine whether there are unsynced save
/// changes (used when recovering from an interrupted / force-quit game session).
///
/// - When `clean_manifest` is available (online), computes the exact SHA-256 diff against the
///   clean manifest. If `last_synced_at` is set, skips hashing files older than `last_synced_at`
///   (with a 1-second grace window for filesystem mtime granularity), as well as new or
///   size-mismatched files, while still reporting all deletions.
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
        let cutoff = last_synced_at.map(|last_sync| {
            last_sync
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or(last_sync)
        });
        let clean_map: HashMap<&str, &opensync_api::ManifestEntry> = manifest
            .files
            .iter()
            .map(|f| (f.path.as_str(), f))
            .collect();

        struct SameSizeCandidate<'a> {
            rel_path: String,
            abs_path: PathBuf,
            size: u64,
            expected_hash: &'a str,
        }

        let mut local_paths = HashSet::new();
        let mut changed: Vec<(String, u64)> = Vec::new();
        let mut to_hash: Vec<SameSizeCandidate<'_>> = Vec::new();

        for entry in WalkDir::new(install_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
        {
            let abs_path = entry.path().to_path_buf();
            let Ok(rel) = abs_path.strip_prefix(install_dir) else {
                continue;
            };
            let rel_path = rel.to_string_lossy().replace('\\', "/");
            if rel_path.ends_with(".part") {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len();
            local_paths.insert(rel_path.clone());

            if let Some(cutoff_time) = cutoff {
                let is_recent = meta
                    .modified()
                    .map(|mtime| mtime >= cutoff_time)
                    .unwrap_or(true);
                if !is_recent {
                    continue;
                }
            }

            match clean_map.get(rel_path.as_str()) {
                None => changed.push((rel_path, size)),
                Some(clean) if clean.size != size => changed.push((rel_path, size)),
                Some(clean) => to_hash.push(SameSizeCandidate {
                    rel_path,
                    abs_path,
                    size,
                    expected_hash: clean.hash.as_str(),
                }),
            }
        }

        if !to_hash.is_empty() {
            let hashed_diffs: Vec<Option<(String, u64)>> = to_hash
                .into_par_iter()
                .map_init(
                    || vec![0u8; HASH_BUF_SIZE],
                    |buf, cand| {
                        let sha256 = hash_file_sha256_with_buf(&cand.abs_path, buf)?;
                        if sha256 != cand.expected_hash {
                            Ok(Some((cand.rel_path, cand.size)))
                        } else {
                            Ok(None)
                        }
                    },
                )
                .collect::<Result<Vec<_>, std::io::Error>>()?;

            changed.extend(hashed_diffs.into_iter().flatten());
        }

        changed.sort_by(|a, b| a.0.cmp(&b.0));

        let mut deletions: Vec<String> = manifest
            .files
            .iter()
            .filter(|f| !local_paths.contains(f.path.as_str()))
            .map(|f| f.path.clone())
            .collect();
        deletions.sort();

        if changed.is_empty() && deletions.is_empty() {
            return Ok(LocalSaveInspection::default());
        }

        let changed_files = changed.len();
        let deleted_files = deletions.len();
        let changed_bytes: u64 = changed.iter().map(|(_, size)| *size).sum();
        let has_changes = changed_files > 0 || deleted_files > 0;
        let mut sample_paths: Vec<String> = changed
            .iter()
            .take(5)
            .map(|(path, _)| path.clone())
            .collect();
        if sample_paths.len() < 5 {
            for del in deletions.iter().take(5 - sample_paths.len()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use opensync_api::ManifestEntry;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    #[test]
    fn hash_file_sha256_and_scan_folder_parallel_hash_large_and_empty_files() {
        let dir = tempdir().unwrap();
        let big = vec![0x5Cu8; HASH_BUF_SIZE * 2 + 12345];
        let small = b"save=42\n".to_vec();

        fs::write(dir.path().join("empty.bin"), &[]).unwrap();
        fs::write(dir.path().join("small.sav"), &small).unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/big.pak"), &big).unwrap();
        fs::write(dir.path().join("ignored.part"), b"partial").unwrap();

        let scanned = scan_folder_parallel(dir.path()).unwrap();
        let paths: Vec<&str> = scanned.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, vec!["empty.bin", "small.sav", "sub/big.pak"]);

        assert_eq!(scanned[0].sha256, hex::encode(Sha256::digest([])));
        assert_eq!(scanned[1].sha256, hex::encode(Sha256::digest(&small)));
        assert_eq!(scanned[2].sha256, hex::encode(Sha256::digest(&big)));
        assert_eq!(
            hash_file_sha256(&dir.path().join("sub/big.pak")).unwrap(),
            scanned[2].sha256
        );
    }

    #[test]
    fn inspect_unsynced_saves_handles_new_size_mismatch_same_size_and_deletions() {
        let dir = tempdir().unwrap();
        let untouched = b"unchanged content".to_vec();
        let same_size_orig = b"AAAA".to_vec();
        let same_size_mod = b"BBBB".to_vec();
        let resized_orig = b"123".to_vec();
        let resized_mod = b"123456".to_vec();

        fs::write(dir.path().join("untouched.bin"), &untouched).unwrap();
        fs::write(dir.path().join("same_size.sav"), &same_size_mod).unwrap();
        fs::write(dir.path().join("resized.sav"), &resized_mod).unwrap();
        fs::write(dir.path().join("new_slot.sav"), b"brand new").unwrap();

        let manifest = GameManifest {
            game_id: 1,
            files: vec![
                ManifestEntry {
                    path: "untouched.bin".into(),
                    hash: hex::encode(Sha256::digest(&untouched)),
                    size: untouched.len() as u64,
                },
                ManifestEntry {
                    path: "same_size.sav".into(),
                    hash: hex::encode(Sha256::digest(&same_size_orig)),
                    size: same_size_orig.len() as u64,
                },
                ManifestEntry {
                    path: "resized.sav".into(),
                    hash: hex::encode(Sha256::digest(&resized_orig)),
                    size: resized_orig.len() as u64,
                },
                ManifestEntry {
                    path: "deleted.tmp".into(),
                    hash: hex::encode(Sha256::digest(b"gone")),
                    size: 4,
                },
            ],
        };

        let res = inspect_unsynced_saves(dir.path(), Some(&manifest), None, None).unwrap();
        assert!(res.has_changes);
        assert_eq!(res.changed_files, 3);
        assert_eq!(res.deleted_files, 1);
        assert_eq!(
            res.changed_bytes,
            (same_size_mod.len() + resized_mod.len() + b"brand new".len()) as u64
        );
        assert_eq!(
            res.sample_paths,
            vec![
                "new_slot.sav",
                "resized.sav",
                "same_size.sav",
                "deleted.tmp (deleted)",
            ]
        );

        // If last_synced_at is well in the future, modified files older than cutoff are ignored,
        // while missing manifest entries still count as deleted.
        let future_sync = SystemTime::now() + Duration::from_secs(60);
        let res_future =
            inspect_unsynced_saves(dir.path(), Some(&manifest), Some(future_sync), None).unwrap();
        assert!(res_future.has_changes);
        assert_eq!(res_future.changed_files, 0);
        assert_eq!(res_future.deleted_files, 1);
    }
}
