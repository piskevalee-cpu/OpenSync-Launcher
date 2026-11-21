mod common;

use common::{MockOpenSyncServer, TEST_CHUNK_SIZE};
use launcher_core::{ConnectionState, LauncherCore, LauncherPaths, ReconnectPolicy};
use launcher_db::{LauncherDb, MemorySecretStore};
use opensync_runner::{LaunchOptions, PostExitSyncStatus};
use opensync_sync::SyncError;
use opensync_transfer::{DownloadControl, TransferError};
use std::fs;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test]
async fn download_streams_multi_megabyte_archive_and_rejects_corrupt_hash() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");

    // 4 MB multi-file game archive
    let big_asset = vec![0x3Cu8; 3 * 1024 * 1024];
    let config = b"resolution=1920x1080\n".to_vec();
    srv.add_game(
        1,
        "FastRunner",
        "fastrunner",
        vec![
            ("assets/textures.pak", big_asset.clone()),
            ("config.ini", config.clone()),
        ],
    );

    core.connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
        .await
        .unwrap();

    let ctrl = DownloadControl::new(0, 0);
    let (installed, outcome) = core.download_game(1, true, ctrl.clone()).await.unwrap();
    assert_eq!(outcome.files.len(), 2);
    assert_eq!(outcome.total_bytes, (big_asset.len() + config.len()) as u64);
    assert!(outcome.avg_bytes_per_sec > 0.0);
    assert_eq!(
        fs::read(installed.install_dir.join("assets/textures.pak")).unwrap(),
        big_asset
    );

    // Corrupt download must be rejected by SHA-256 verification and leave no staging leftovers
    srv.add_game(
        2,
        "CorruptGame",
        "corrupt-game",
        vec![("main.bin", b"clean bytes".to_vec())],
    );
    srv.set_corrupt_download(2, "main.bin");

    let err = core
        .download_game(2, true, DownloadControl::new(0, 0))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        launcher_core::CoreError::Transfer(TransferError::HashMismatch { .. })
    ));
}

#[tokio::test]
async fn sync_uploads_new_modified_multi_chunk_and_deleted_files_with_mass_delete_guard() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");
    srv.add_game(
        5,
        "SyncQuest",
        "syncquest",
        vec![
            ("game.bin", b"original binary".to_vec()),
            ("settings.cfg", b"volume=50\n".to_vec()),
            ("old_cache.tmp", b"remove me".to_vec()),
            ("readme.txt", b"keep me".to_vec()),
        ],
    );

    core.connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
        .await
        .unwrap();
    let (installed, _) = core
        .download_game(5, true, DownloadControl::new(0, 0))
        .await
        .unwrap();

    // Modify 1 file, add 1 multi-chunk save (> 2.5x TEST_CHUNK_SIZE), delete 1 file (25% <= 50%)
    let multi_chunk_save = vec![0x77u8; TEST_CHUNK_SIZE * 2 + 12_345];
    fs::write(installed.install_dir.join("settings.cfg"), b"volume=90\n").unwrap();
    fs::create_dir_all(installed.install_dir.join("saves")).unwrap();
    fs::write(
        installed.install_dir.join("saves/slot1.sav"),
        &multi_chunk_save,
    )
    .unwrap();
    fs::remove_file(installed.install_dir.join("old_cache.tmp")).unwrap();

    let report = core.sync_game_now(5, false).await.unwrap();
    assert_eq!(report.uploaded_files, 2);
    assert_eq!(report.deleted_paths, 1);
    assert_eq!(report.skipped_unchanged, 2);

    let ov = srv.overlay_files(1, 5);
    assert_eq!(ov.get("settings.cfg").unwrap(), b"volume=90\n");
    assert_eq!(ov.get("saves/slot1.sav").unwrap(), &multi_chunk_save);
    assert_eq!(srv.overlay_deletions(1, 5), vec!["old_cache.tmp"]);

    // Mass-deletion guard: deleting >50% of manifest files without force must be blocked
    fs::remove_file(installed.install_dir.join("game.bin")).unwrap();
    fs::remove_file(installed.install_dir.join("readme.txt")).unwrap();
    let guarded = core.sync_game_now(5, false).await.unwrap_err();
    assert!(matches!(
        guarded,
        launcher_core::CoreError::Sync(SyncError::MassDeletionGuard { .. })
    ));
}

#[tokio::test]
async fn runner_auto_syncs_saves_every_time_game_closes_and_flushes_offline_queue_on_reconnect() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");

    // Game script writes a fresh save file every time it runs, then exits
    let script = b"#!/bin/sh\nmkdir -p saves\necho \"progress=$1\" > saves/auto.sav\n";
    srv.add_game(
        7,
        "AutoSyncGame",
        "autosync-game",
        vec![
            ("play.sh", script.to_vec()),
            ("assets/base.dat", b"unchanged asset".to_vec()),
        ],
    );

    core.connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
        .await
        .unwrap();
    core.download_game(7, true, DownloadControl::new(0, 0))
        .await
        .unwrap();

    // Session 1 (online): game writes progress=chapter1 and closes -> auto-syncs immediately
    let out1 = core
        .launch_game(
            7,
            LaunchOptions {
                auto_sync_on_close: true,
                sync_settle_delay: Duration::from_millis(20),
                heartbeat_interval: Duration::from_millis(100),
                extra_args: vec!["chapter1".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(out1.exit_code, Some(0));
    assert!(out1.session.synced);
    match out1.sync_status {
        PostExitSyncStatus::Synced(rep) => {
            assert_eq!(rep.uploaded_files, 1);
            assert_eq!(rep.deleted_paths, 0);
        }
        other => panic!("expected Synced status on close, got {other:?}"),
    }
    assert_eq!(
        srv.overlay_files(1, 7).get("saves/auto.sav").unwrap(),
        b"progress=chapter1\n"
    );
    assert_eq!(srv.sync_count(), 1);

    // Session 2 (server goes offline while playing): game writes progress=chapter2 and closes ->
    // queued cleanly in pending_syncs, then auto-flushed when server comes back and reconnects!
    srv.set_offline(true);
    let out2 = core
        .launch_game(
            7,
            LaunchOptions {
                auto_sync_on_close: true,
                sync_settle_delay: Duration::from_millis(20),
                heartbeat_interval: Duration::from_millis(100),
                extra_args: vec!["chapter2".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        out2.sync_status,
        PostExitSyncStatus::QueuedOffline(_)
    ));

    // Bring server back online and run auto-reconnect -> queued save flushes automatically!
    srv.set_offline(false);
    let rec = core
        .reconnect_with_backoff(ReconnectPolicy {
            initial_delay: Duration::from_millis(20),
            max_delay: Duration::from_millis(50),
            max_attempts: 5,
        })
        .await
        .unwrap();
    assert_eq!(core.connection_state().await, ConnectionState::Connected);
    assert_eq!(rec.flushed_syncs, 1);
    assert_eq!(
        srv.overlay_files(1, 7).get("saves/auto.sav").unwrap(),
        b"progress=chapter2\n"
    );
    assert_eq!(srv.sync_count(), 2);

    let pt = core.playtime_summary(7).await.unwrap();
    assert_eq!(pt.session_count, 2);
}
