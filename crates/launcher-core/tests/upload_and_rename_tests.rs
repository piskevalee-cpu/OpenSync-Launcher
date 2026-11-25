mod common;

use common::{MockOpenSyncServer, TEST_CHUNK_SIZE};
use launcher_core::{LauncherCore, LauncherEvent, LauncherPaths, UploadJobSnapshot, UploadStatus};
use launcher_db::{LauncherDb, MemorySecretStore, Profile};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::broadcast;

const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, 1, 2, 3, 4];

fn build_core(root: &Path) -> Arc<LauncherCore> {
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let paths = LauncherPaths::under(root);
    Arc::new(LauncherCore::new(db, secrets, paths))
}

async fn connect(core: &Arc<LauncherCore>, srv: &MockOpenSyncServer) -> Profile {
    core.connect_with_password("Home", &srv.base_url, "alice", "pass1234")
        .await
        .unwrap()
}

async fn job_snapshot(core: &LauncherCore, job_id: &str) -> Option<UploadJobSnapshot> {
    core.list_uploads()
        .await
        .into_iter()
        .find(|job| job.id == job_id)
}

async fn wait_for_status(
    core: &LauncherCore,
    job_id: &str,
    target: UploadStatus,
    limit: Duration,
) -> UploadJobSnapshot {
    tokio::time::timeout(limit, async {
        loop {
            if let Some(job) = job_snapshot(core, job_id).await
                && job.status == target
            {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("upload {job_id} never reached {target:?}"))
}

fn write_file(root: &Path, rel: &str, data: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, data).unwrap();
}

#[tokio::test]
async fn change_username_updates_server_db_and_active_profile() {
    let dir = tempdir().unwrap();
    let core = build_core(dir.path());
    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "alice", "pass1234");
    srv.add_user(2, "bob", "pass1234");
    let alice = connect(&core, &srv).await;

    let mut events = core.subscribe_events();
    let updated = core.change_username("  new_name  ").await.unwrap();
    assert_eq!(updated.username, "new_name");
    assert_eq!(core.active_profile().await.unwrap().username, "new_name");
    assert_eq!(
        core.db()
            .get_profile(&alice.id)
            .unwrap()
            .unwrap()
            .username,
        "new_name"
    );
    assert!(srv.username_exists("new_name"));
    assert!(!srv.username_exists("alice"));

    let saw_update = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(LauncherEvent::ProfileUpdated { profile }) => {
                    return profile.username == "new_name";
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap();
    assert!(saw_update, "expected ProfileUpdated after username change");

    // Case-insensitive collision on the server surfaces its message verbatim.
    let err = core.change_username("BOB").await.unwrap_err();
    assert!(
        format!("{err}").contains("username already taken"),
        "unexpected error: {err}"
    );
    assert_eq!(core.active_profile().await.unwrap().username, "new_name");
    assert_eq!(
        core.db()
            .get_profile(&alice.id)
            .unwrap()
            .unwrap()
            .username,
        "new_name"
    );
}

#[tokio::test]
async fn folder_upload_is_parallel_chunked_and_indexed() {
    let dir = tempdir().unwrap();
    let core = build_core(dir.path());
    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "alice", "pass1234");
    connect(&core, &srv).await;

    let folder = dir.path().join("game_src");
    let big = vec![0x5A; TEST_CHUNK_SIZE * 3 + 123]; // 4 chunks
    let medium = vec![0x33; TEST_CHUNK_SIZE * 2 + 7]; // 3 chunks
    let small = b"hello world".to_vec();
    write_file(&folder, "big.bin", &big);
    write_file(&folder, "nested/medium.bin", &medium);
    write_file(&folder, "small.txt", &small);
    write_file(&folder, "empty.dat", &[]);
    write_file(&folder, ".DS_Store", b"hidden");
    write_file(&folder, "ignored.part", b"partial");
    write_file(&folder, ".git/config", b"git");

    let expected_total = (big.len() + medium.len() + small.len()) as u64;
    let job_id = core
        .start_game_upload(
            "Big Game",
            "a description",
            "a GPU",
            Some(folder.to_str().unwrap()),
            Some((JPG.to_vec(), "image/jpeg".to_string())),
        )
        .await
        .unwrap();

    let snap = wait_for_status(&core, &job_id, UploadStatus::Ready, Duration::from_secs(30)).await;
    assert_eq!(snap.files_uploaded, 4);
    assert_eq!(snap.total_files, 4);
    assert_eq!(snap.bytes_uploaded, expected_total);
    assert_eq!(snap.total_bytes, expected_total);

    let (files, cover, status) = {
        let st = srv.state.lock().unwrap();
        let id = *st.games.keys().next().expect("game was created");
        (
            st.games.get(&id).unwrap().files.clone(),
            st.covers.get(&id).cloned(),
            st.game_status.get(&id).cloned(),
        )
    };
    assert_eq!(files.len(), 4, "hidden and .part files must be skipped");
    assert_eq!(files.get("big.bin").unwrap(), &big);
    assert_eq!(files.get("nested/medium.bin").unwrap(), &medium);
    assert_eq!(files.get("small.txt").unwrap(), &small);
    assert_eq!(files.get("empty.dat").unwrap().len(), 0);
    assert_eq!(cover.unwrap().0, JPG);
    assert_eq!(status.as_deref(), Some("ready"));
}

#[tokio::test]
async fn folder_upload_pause_resume_and_cancel() {
    let dir = tempdir().unwrap();
    let core = build_core(dir.path());
    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "alice", "pass1234");
    connect(&core, &srv).await;

    let folder = dir.path().join("pause_src");
    fs::create_dir_all(&folder).unwrap();
    let payload = vec![0x77; TEST_CHUNK_SIZE * 16];
    fs::write(folder.join("payload.bin"), &payload).unwrap();
    srv.set_chunk_delay_ms(8);

    let job_id = core
        .start_game_upload(
            "Pause Game",
            "",
            "",
            Some(folder.to_str().unwrap()),
            None,
        )
        .await
        .unwrap();
    let _ = wait_for_status(&core, &job_id, UploadStatus::Uploading, Duration::from_secs(5)).await;
    assert!(core.pause_upload(&job_id).await);
    assert_eq!(
        job_snapshot(&core, &job_id).await.unwrap().status,
        UploadStatus::Paused
    );
    let bytes_at_pause = job_snapshot(&core, &job_id).await.unwrap().bytes_uploaded;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let still_paused = job_snapshot(&core, &job_id).await.unwrap();
    assert_eq!(still_paused.status, UploadStatus::Paused);
    assert!(still_paused.bytes_uploaded <= bytes_at_pause + TEST_CHUNK_SIZE as u64);
    assert!(core.resume_upload(&job_id).await);
    let done = wait_for_status(&core, &job_id, UploadStatus::Ready, Duration::from_secs(30)).await;
    assert_eq!(done.bytes_uploaded, payload.len() as u64);
    assert_eq!(done.total_bytes, payload.len() as u64);

    // A cancelled upload is removed from the server (best-effort cleanup in the task).
    let cancel_folder = dir.path().join("cancel_src");
    fs::create_dir_all(&cancel_folder).unwrap();
    fs::write(cancel_folder.join("cancel.bin"), vec![0x11; TEST_CHUNK_SIZE * 16]).unwrap();
    srv.set_chunk_delay_ms(10);
    let cancel_job = core
        .start_game_upload(
            "Cancel Game",
            "",
            "",
            Some(cancel_folder.to_str().unwrap()),
            None,
        )
        .await
        .unwrap();
    let _ = wait_for_status(&core, &cancel_job, UploadStatus::Uploading, Duration::from_secs(5))
        .await;
    assert!(core.cancel_upload(&cancel_job).await);
    let cancelled =
        wait_for_status(&core, &cancel_job, UploadStatus::Cancelled, Duration::from_secs(5)).await;
    let game_id = cancelled.game_id.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while srv.game_files(game_id).is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled game must be deleted from the server");
    assert!(!core.pause_upload(&cancel_job).await);
}

#[tokio::test]
async fn picked_folder_upload_moves_into_games_dir_and_marks_installed() {
    let dir = tempdir().unwrap();
    let core = build_core(dir.path());
    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "alice", "pass1234");
    let profile = connect(&core, &srv).await;

    // A user folder outside games_dir, with an executable and a save file.
    let source = dir.path().join("picked_game_folder");
    write_file(&source, "mygame.sh", b"#!/bin/sh\necho mygame\n");
    write_file(&source, "saves/slot1.dat", b"initial save\n");

    let job_id = core
        .start_game_upload(
            "My Game",
            "",
            "",
            Some(source.to_str().unwrap()),
            None,
        )
        .await
        .unwrap();
    let done = wait_for_status(&core, &job_id, UploadStatus::Ready, Duration::from_secs(30)).await;
    let game_id = done.game_id.expect("ready upload must have a game id");

    let slug = {
        let st = srv.state.lock().unwrap();
        st.games.get(&game_id).unwrap().slug.clone()
    };
    let expected_dir = dir.path().join("games").join(&profile.id).join(&slug);

    // The files were moved, not copied: the original picked folder no longer exists.
    assert!(expected_dir.join("mygame.sh").is_file());
    assert_eq!(
        fs::read(expected_dir.join("saves/slot1.dat")).unwrap(),
        b"initial save\n"
    );
    assert!(
        !source.exists(),
        "picked source folder must be moved, not left behind"
    );

    // The game is registered with discovered executable and clean manifest hash.
    let installed = core
        .db()
        .get_installed_game(&profile.id, game_id)
        .unwrap()
        .expect("uploaded game must be installed");
    assert_eq!(
        installed.install_dir,
        fs::canonicalize(&expected_dir).unwrap()
    );
    assert_eq!(installed.executable_rel_path.as_deref(), Some("mygame.sh"));
    assert!(installed.manifest_hash.is_some(), "clean manifest hash recorded");
    assert!(installed.total_size > 0);
    assert!(
        core.list_downloaded_games()
            .await
            .unwrap()
            .iter()
            .any(|g| g.game_id == game_id),
        "installed game must be immediately visible in the library"
    );
}

#[tokio::test]
async fn dropped_upload_stages_chunks_then_finalizes() {
    let dir = tempdir().unwrap();
    let core = build_core(dir.path());
    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "alice", "pass1234");
    let profile = connect(&core, &srv).await;
    srv.set_chunk_delay_ms(0);

    let job_id = core
        .start_game_upload("Dropped Game", "", "", None, None)
        .await
        .unwrap();
    let initial = job_snapshot(&core, &job_id).await.unwrap();
    assert_eq!(initial.status, UploadStatus::Queued);
    let game_id = initial.game_id.unwrap();
    let staging: PathBuf = dir.path().join("cache").join("uploads").join(&job_id);
    assert!(staging.join("files").is_dir());

    let payload = vec![0xAB; TEST_CHUNK_SIZE * 2 + 55];
    let total = 3usize;
    for index in 0..total {
        let offset = index * TEST_CHUNK_SIZE;
        let len = TEST_CHUNK_SIZE.min(payload.len() - offset);
        let last = core
            .stage_upload_chunk(
                &job_id,
                "saves/slot1.dat",
                index,
                total,
                payload.len() as u64,
                &payload[offset..offset + len],
            )
            .unwrap();
        assert_eq!(last, index + 1 == total);
    }
    assert!(staging.join("files").join("saves").join("slot1.dat").is_file());
    assert!(core
        .stage_upload_chunk(&job_id, "../escape.dat", 0, 1, 1, b"x")
        .is_err());
    assert!(core
        .stage_upload_chunk(&job_id, "saves/slot1.dat", 0, total, payload.len() as u64, b"x")
        .is_err());

    core.finalize_dropped_upload(&job_id).await.unwrap();
    let done = wait_for_status(&core, &job_id, UploadStatus::Ready, Duration::from_secs(30)).await;
    assert_eq!(done.files_uploaded, 1);
    assert_eq!(done.total_files, 1);
    assert_eq!(done.total_bytes, payload.len() as u64);
    let files: HashMap<String, Vec<u8>> = srv.game_files(game_id).unwrap();
    assert_eq!(files.get("saves/slot1.dat").unwrap(), &payload);
    assert!(!staging.exists(), "staging directory must be cleaned up");

    // Staged files were moved out of staging into the games dir and installed.
    let installed = core
        .db()
        .get_installed_game(&profile.id, game_id)
        .unwrap()
        .expect("dropped upload must be installed");
    assert_eq!(
        fs::read(installed.install_dir.join("saves/slot1.dat")).unwrap(),
        payload
    );
    let expected_games = std::fs::canonicalize(dir.path().join("games")).unwrap();
    assert!(installed.install_dir.starts_with(expected_games));
}
