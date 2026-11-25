mod common;

use common::MockOpenSyncServer;
use launcher_core::{LauncherCore, LauncherEvent, LauncherPaths};
use launcher_db::{LauncherDb, MemorySecretStore};
use opensync_transfer::DownloadControl;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::broadcast;

async fn wait_for_background_sync(
    rx: &mut broadcast::Receiver<LauncherEvent>,
    predicate: impl Fn(usize, usize) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match rx.recv().await {
                Ok(LauncherEvent::BackgroundSyncCompleted {
                    synced_games,
                    flushed_pending,
                }) if predicate(synced_games, flushed_pending) => return,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    panic!("event channel closed while waiting for BackgroundSyncCompleted")
                }
            }
        }
    })
    .await
    .expect("timed out waiting for the expected background sync event");
}

#[tokio::test]
async fn background_loop_recovers_offline_queue_and_syncs_external_save_edits() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = Arc::new(LauncherCore::new(db, secrets, LauncherPaths::under(dir.path())));

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");
    srv.add_game(
        21,
        "BackgroundQuest",
        "background-quest",
        vec![
            ("game.bin", b"binary".to_vec()),
            ("saves/slot1.sav", b"old save".to_vec()),
        ],
    );

    let profile = core
        .connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
        .await
        .unwrap();
    let (installed, _) = core
        .download_game(21, true, DownloadControl::new(0, 0))
        .await
        .unwrap();

    // A save change made while the server is down is queued for later.
    srv.set_offline(true);
    fs::write(installed.install_dir.join("saves/slot1.sav"), b"offline progress").unwrap();
    fs::write(installed.install_dir.join("saves/slot2.sav"), b"new offline save").unwrap();
    core.db()
        .queue_pending_sync(&profile.id, 21, "server unreachable")
        .unwrap();

    let mut events = core.subscribe_events();
    srv.set_offline(false);

    // The periodic background loop probes connectivity, reconnects and flushes the queue.
    let handle = core.spawn_background_auto_sync(Duration::from_millis(40));
    wait_for_background_sync(&mut events, |_, flushed| flushed >= 1).await;
    handle.abort();

    let overlay = srv.overlay_files(1, 21);
    assert_eq!(overlay.get("saves/slot1.sav").unwrap(), b"offline progress");
    assert_eq!(overlay.get("saves/slot2.sav").unwrap(), b"new offline save");
    assert!(core.list_pending_syncs().await.unwrap().is_empty());
    assert!(core.db().get_installed_game(&profile.id, 21).unwrap().unwrap().last_synced_at.is_some());
    assert_eq!(core.connection_state().await, launcher_core::ConnectionState::Connected);

    // An external edit while online is picked up by the next pass without any manual click.
    fs::write(installed.install_dir.join("saves/slot3.sav"), b"external edit").unwrap();
    let outcome = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(outcome.synced_games, 1);
    assert_eq!(outcome.flushed_pending, 0);
    assert_eq!(
        srv.overlay_files(1, 21).get("saves/slot3.sav").unwrap(),
        b"external edit"
    );
    assert!(srv.sync_count() >= 2);
    wait_for_background_sync(&mut events, |synced, flushed| synced == 1 && flushed == 0).await;
}

#[tokio::test]
async fn background_sync_skips_currently_playing_games_and_their_pending_queue() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");
    srv.add_game(
        22,
        "LiveSession",
        "live-session",
        vec![("game.bin", b"binary".to_vec())],
    );

    let profile = core
        .connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
        .await
        .unwrap();
    let (installed, _) = core
        .download_game(22, true, DownloadControl::new(0, 0))
        .await
        .unwrap();

    fs::create_dir_all(installed.install_dir.join("saves")).unwrap();
    fs::write(installed.install_dir.join("saves/live.sav"), b"written by the game").unwrap();
    core.db()
        .queue_pending_sync(&profile.id, 22, "queued while playing")
        .unwrap();
    core.mark_game_playing(&profile.id, 22);
    assert!(core.is_game_playing(&profile.id, 22));

    let skipped = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(skipped.synced_games, 0);
    assert_eq!(skipped.flushed_pending, 0);
    assert!(srv.overlay_files(1, 22).is_empty());
    assert_eq!(srv.sync_count(), 0);
    assert_eq!(core.list_pending_syncs().await.unwrap().len(), 1);
    assert!(core
        .db()
        .get_installed_game(&profile.id, 22)
        .unwrap()
        .unwrap()
        .last_synced_at
        .is_none());

    // Once the session ends the queue is flushed and the save is uploaded automatically.
    core.mark_game_stopped(&profile.id, 22);
    assert!(!core.is_game_playing(&profile.id, 22));
    let synced = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(synced.flushed_pending, 1);
    assert!(synced.synced_games >= 1);
    assert_eq!(
        srv.overlay_files(1, 22).get("saves/live.sav").unwrap(),
        b"written by the game"
    );
    assert!(srv.sync_count() >= 1);
    assert!(core.list_pending_syncs().await.unwrap().is_empty());
    assert!(core
        .db()
        .get_installed_game(&profile.id, 22)
        .unwrap()
        .unwrap()
        .last_synced_at
        .is_some());
}

#[tokio::test]
async fn background_sync_skips_idle_games_until_external_edit_or_same_size_mtime_change() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "valerio", "secret123");
    srv.add_game(
        23,
        "IdleSkipQuest",
        "idle-skip-quest",
        vec![
            ("game.bin", b"binary".to_vec()),
            ("saves/slot1.sav", b"save-001".to_vec()),
        ],
    );

    let profile = core
        .connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
        .await
        .unwrap();
    let (installed, _) = core
        .download_game(23, true, DownloadControl::new(0, 0))
        .await
        .unwrap();

    // First background pass runs sync_game_saves (no fingerprint cached yet) and records it.
    let first = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(first.synced_games, 1);
    assert_eq!(first.flushed_pending, 0);
    let synced_at_after_first = core
        .db()
        .get_installed_game(&profile.id, 23)
        .unwrap()
        .unwrap()
        .last_synced_at
        .expect("last_synced_at must be set after initial pass");

    // Second idle pass skips sync_game_saves completely (zero HTTP/hashing) when unchanged.
    tokio::time::sleep(Duration::from_millis(15)).await;
    let second = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(second.synced_games, 0);
    assert_eq!(second.flushed_pending, 0);
    assert_eq!(
        core.db()
            .get_installed_game(&profile.id, 23)
            .unwrap()
            .unwrap()
            .last_synced_at,
        Some(synced_at_after_first),
        "idle skip must not invoke sync_game_saves or bump last_synced_at"
    );

    // Even with `.part` temp files present and server offline while session is Connected,
    // idle skip avoids any HTTP manifest fetch.
    fs::write(installed.install_dir.join("saves/temp.sav.part"), b"partial").unwrap();
    srv.set_offline(true);
    let while_offline = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(while_offline.synced_games, 0);
    assert_eq!(while_offline.flushed_pending, 0);
    assert_eq!(
        core.connection_state().await,
        launcher_core::ConnectionState::Connected
    );
    srv.set_offline(false);

    // Same-size external edit that updates mtime must invalidate the idle skip and trigger sync.
    tokio::time::sleep(Duration::from_millis(25)).await;
    fs::write(installed.install_dir.join("saves/slot1.sav"), b"save-002").unwrap();
    let after_same_size_edit = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(after_same_size_edit.synced_games, 1);
    assert_eq!(
        srv.overlay_files(1, 23).get("saves/slot1.sav").unwrap(),
        b"save-002"
    );

    // Next idle pass skips again.
    let idle_again = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(idle_again.synced_games, 0);

    // Adding a brand new file also triggers sync.
    fs::write(installed.install_dir.join("saves/slot2.sav"), b"new slot").unwrap();
    let after_new_file = core.run_background_auto_sync_once().await.unwrap();
    assert_eq!(after_new_file.synced_games, 1);
    assert_eq!(
        srv.overlay_files(1, 23).get("saves/slot2.sav").unwrap(),
        b"new slot"
    );
}
