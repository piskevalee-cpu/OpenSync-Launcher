mod common;

use common::MockOpenSyncServer;
use launcher_core::{ConnectionState, LauncherCore, LauncherPaths, ReconnectPolicy};
use launcher_db::{FileSecretStore, LauncherDb, MemorySecretStore};
use opensync_transfer::DownloadControl;
use std::time::Duration;
use tempfile::tempdir;

#[tokio::test]
async fn connection_remembers_active_profile_and_downloaded_games_across_restarts() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("launcher.db");
    let secrets_path = dir.path().join("secrets.json");
    let paths = LauncherPaths::under(dir.path());

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");
    srv.add_game(
        10,
        "Fogwoods",
        "fogwoods",
        vec![
            ("fogwoods.sh", b"#!/bin/sh\necho ok\n".to_vec()),
            ("data/map.bin", vec![0x42u8; 32 * 1024]),
        ],
    );

    // First run: sign in and download a game
    let profile_id = {
        let db = LauncherDb::open(&db_path).unwrap();
        let secrets = FileSecretStore::new(&secrets_path);
        let core = LauncherCore::new(db, secrets, paths.clone());

        let profile = core
            .connect_with_password("Home LAN", &srv.base_url, "valerio", "secret123")
            .await
            .unwrap();
        assert_eq!(core.connection_state().await, ConnectionState::Connected);

        let (installed, outcome) = core
            .download_game(10, true, DownloadControl::new(0, 0))
            .await
            .unwrap();
        assert_eq!(installed.game_id, 10);
        assert_eq!(installed.executable_rel_path.as_deref(), Some("fogwoods.sh"));
        assert_eq!(outcome.files.len(), 2);
        profile.id
    };

    // Second run (simulated full restart): recreate LauncherCore from disk
    let db2 = LauncherDb::open(&db_path).unwrap();
    let secrets2 = FileSecretStore::new(&secrets_path);
    let core2 = LauncherCore::new(db2, secrets2, paths);

    let restored = core2
        .restore_remembered_profile()
        .await
        .unwrap()
        .expect("must remember active profile across restart");
    assert_eq!(restored.id, profile_id);
    assert_eq!(restored.username, "valerio");
    assert_eq!(core2.connection_state().await, ConnectionState::Connected);

    let downloaded = core2.list_downloaded_games().await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert_eq!(downloaded[0].name, "Fogwoods");
    assert!(downloaded[0].install_dir.join("data/map.bin").is_file());
    assert!(downloaded[0].manifest_hash.is_some());
}

#[tokio::test]
async fn connection_switches_profiles_and_isolates_catalog_and_downloaded_games() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

    let srv_a = MockOpenSyncServer::start().await;
    srv_a.add_user(1, "alice", "passA123");
    srv_a.add_game(
        1,
        "Hollow Knight",
        "hollow-knight",
        vec![("hk.sh", b"#!/bin/sh\n".to_vec())],
    );

    let srv_b = MockOpenSyncServer::start().await;
    srv_b.add_user(1, "bob", "passB123");
    srv_b.add_game(
        99,
        "Celeste",
        "celeste",
        vec![("celeste.sh", b"#!/bin/sh\n".to_vec())],
    );

    // Connect to Profile A and download Hollow Knight
    let prof_a = core
        .connect_with_password("Home", &srv_a.base_url, "alice", "passA123")
        .await
        .unwrap();
    core.download_game(1, true, DownloadControl::new(0, 0))
        .await
        .unwrap();
    assert_eq!(core.list_downloaded_games().await.unwrap().len(), 1);

    // Connect to Profile B
    let prof_b = core
        .connect_with_password("Friend LAN", &srv_b.base_url, "bob", "passB123")
        .await
        .unwrap();
    assert_eq!(core.active_profile().await.unwrap().username, "bob");
    assert_eq!(core.list_downloaded_games().await.unwrap().len(), 0);
    let remote_b = core.fetch_remote_games().await.unwrap();
    assert_eq!(remote_b.len(), 1);
    assert_eq!(remote_b[0].name, "Celeste");

    // Switch back to Profile A: session, remote catalog, and downloaded games all restore cleanly
    core.switch_profile(&prof_a.id).await.unwrap();
    assert_eq!(core.active_profile().await.unwrap().username, "alice");
    assert_eq!(core.connection_state().await, ConnectionState::Connected);
    let dl_a = core.list_downloaded_games().await.unwrap();
    assert_eq!(dl_a.len(), 1);
    assert_eq!(dl_a[0].name, "Hollow Knight");
    let remote_a = core.fetch_remote_games().await.unwrap();
    assert_eq!(remote_a[0].name, "Hollow Knight");

    // Switch back to Profile B again
    core.switch_profile(&prof_b.id).await.unwrap();
    assert_eq!(core.active_profile().await.unwrap().username, "bob");
    assert_eq!(core.list_profiles().unwrap().len(), 2);
}

#[tokio::test]
async fn connection_auto_reconnects_with_backoff_when_server_recovers() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");
    core.connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
        .await
        .unwrap();

    // Take server offline
    srv.set_offline(true);
    assert_eq!(core.probe_connection().await.unwrap(), false);
    assert_eq!(core.connection_state().await, ConnectionState::Disconnected);

    // Bring server back online after 120ms while auto-reconnect is retrying
    let offline_flag = srv.offline.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        offline_flag.store(false, std::sync::atomic::Ordering::SeqCst);
    });

    let outcome = core
        .reconnect_with_backoff(ReconnectPolicy {
            initial_delay: Duration::from_millis(40),
            max_delay: Duration::from_millis(100),
            max_attempts: 10,
        })
        .await
        .unwrap();

    assert!(
        outcome.attempts >= 2,
        "should have retried while offline, got {} attempts",
        outcome.attempts
    );
    assert_eq!(core.connection_state().await, ConnectionState::Connected);
}
