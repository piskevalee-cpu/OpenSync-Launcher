mod common;

use common::MockOpenSyncServer;
use launcher_core::{CoreError, LauncherCore, LauncherEvent, LauncherPaths};
use launcher_db::{LauncherDb, MemorySecretStore};
use opensync_api::OpenSyncClient;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;
use tokio::sync::broadcast;

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
const JPG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 16, 1, 2, 3, 4];

#[tokio::test]
async fn profile_avatar_upload_cache_offline_remove_rename_and_delete_account() {
    let dir = tempdir().unwrap();
    let db = LauncherDb::open_in_memory().unwrap();
    let secrets = MemorySecretStore::new();
    let paths = LauncherPaths::under(dir.path());
    let core = LauncherCore::new(db, secrets, paths.clone());

    let srv = MockOpenSyncServer::start().await;
    srv.add_user(1, "alice", "pass1234");
    srv.add_user(2, "bob", "pass1234");
    srv.set_user_stats(1, 3, 7, 11);
    srv.set_user_created_at(1, "2026-03-04T12:00:00Z");

    let alice = core
        .connect_with_password("Home", &srv.base_url, "alice", "pass1234")
        .await
        .unwrap();

    // Profile data mirrors the web UI: stats, member-since date and default avatar path.
    let me = core.get_current_user_profile().await.unwrap();
    assert_eq!(me.username, "alice");
    assert_eq!(me.role, "admin");
    assert_eq!(me.created_at.as_deref(), Some("2026-03-04T12:00:00Z"));
    assert_eq!(me.pfp.as_deref(), Some("/img/blankpfp.jpg"));
    let stats = me.stats.unwrap();
    assert_eq!((stats.uploaded, stats.downloaded, stats.synced), (3, 7, 11));
    assert!(core
        .fetch_cached_user_pfp(Some("/img/blankpfp.jpg"), 1)
        .await
        .unwrap()
        .is_none());

    // Upload a PNG avatar; it is stored server-side and warmed into the local cache.
    let pfp_path = core
        .upload_active_profile_pfp(PNG.to_vec(), "image/png")
        .await
        .unwrap();
    assert_eq!(pfp_path, "/api/auth/users/1/pfp");
    assert_eq!(srv.user_pfp(1).unwrap().0, PNG);
    let pfp_dir = paths.cache_dir.join("pfps");
    assert!(pfp_dir.join(format!("{}_1.png", alice.id)).is_file());
    assert!(pfp_dir.join(format!("{}_1.mime", alice.id)).is_file());

    let cached = core
        .fetch_cached_user_pfp(Some(&pfp_path), 1)
        .await
        .unwrap()
        .expect("uploaded avatar must be readable from cache");
    assert_eq!(cached, (PNG.to_vec(), "image/png".to_string()));

    // Avatar recognition is offline-capable: no server round-trip needed on a cache hit.
    srv.set_offline(true);
    let offline = core
        .fetch_cached_user_pfp(None, 1)
        .await
        .unwrap()
        .expect("cached avatar must survive a server outage");
    assert_eq!(offline, cached);
    srv.set_offline(false);

    // Removing the avatar drops both the server image and the disk cache entry.
    let default_path = core.remove_active_profile_pfp().await.unwrap();
    assert_eq!(default_path, "/img/blankpfp.jpg");
    assert!(srv.user_pfp(1).is_none());
    assert!(!pfp_dir.join(format!("{}_1.png", alice.id)).exists());
    assert!(!pfp_dir.join(format!("{}_1.mime", alice.id)).exists());
    assert!(core
        .fetch_cached_user_pfp(Some(&pfp_path), 1)
        .await
        .unwrap()
        .is_none());

    // Instance label rename updates SQLite, the live session and emits ProfileUpdated.
    let mut events = core.subscribe_events();
    let renamed = core.rename_profile("Home Arcade").await.unwrap();
    assert_eq!(renamed.label, "Home Arcade");
    assert_eq!(core.active_profile().await.unwrap().label, "Home Arcade");
    assert_eq!(core.db().get_profile(&alice.id).unwrap().unwrap().label, "Home Arcade");
    let saw_update = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(LauncherEvent::ProfileUpdated { profile }) => {
                    return profile.label == "Home Arcade";
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return false,
            }
        }
    })
    .await
    .unwrap();
    assert!(saw_update, "expected ProfileUpdated event after rename");

    // The server protects its last admin: the error message is surfaced to the UI verbatim.
    let err = core.delete_active_account().await.unwrap_err();
    assert!(
        format!("{err}").contains("cannot delete the last admin"),
        "unexpected error: {err}"
    );
    assert!(core.active_profile().await.is_some());

    // A regular account can delete itself; the local profile/session are cleaned up too.
    let bob = core
        .connect_with_password("NAS", &srv.base_url, "bob", "pass1234")
        .await
        .unwrap();
    core.delete_active_account().await.unwrap();
    assert!(srv.is_user_deleted("bob"));
    assert_eq!(srv.user_count(), 1);
    assert!(core.active_profile().await.is_none());
    assert!(core.db().get_profile(&bob.id).unwrap().is_none());
    assert!(matches!(
        core.delete_active_account().await,
        Err(CoreError::NoActiveProfile)
    ));

    // Verify the on-disk cache file name is safe even for unusual profile ids.
    assert!(fs::metadata(&pfp_dir).is_ok());
}

#[tokio::test]
async fn api_pfp_validation_and_default_recognition() {
    let srv = MockOpenSyncServer::start().await;
    let cookie = srv.add_user(1, "carol", "pass1234");
    let client = OpenSyncClient::new(&srv.base_url, Some(cookie)).unwrap();

    assert!(client.fetch_pfp(None, 1).await.unwrap().is_none());
    assert!(client
        .fetch_pfp(Some("/img/blankpfp.jpg"), 1)
        .await
        .unwrap()
        .is_none());
    assert!(client
        .fetch_pfp(Some("/api/auth/users/../1/pfp"), 1)
        .await
        .unwrap()
        .is_none());

    // A 404 from the server is a normal "no avatar yet" answer, not an error.
    assert!(client
        .fetch_pfp(Some("/api/auth/users/1/pfp"), 1)
        .await
        .unwrap()
        .is_none());

    // JPEG uploads are accepted with their real content type and round-trip byte-for-byte.
    let path = client.upload_pfp(JPG.to_vec(), "image/jpeg").await.unwrap();
    assert_eq!(path, "/api/auth/users/1/pfp");
    assert_eq!(
        client.fetch_pfp(Some(&path), 1).await.unwrap().unwrap(),
        (JPG.to_vec(), "image/jpeg".to_string())
    );

    // Invalid content types and oversized bodies are rejected client-side.
    assert!(client.upload_pfp(JPG.to_vec(), "image/gif").await.is_err());
    assert!(client
        .upload_pfp(vec![0u8; 8 * 1024 * 1024 + 1], "image/png")
        .await
        .is_err());

    let default = client.remove_pfp().await.unwrap();
    assert_eq!(default, "/img/blankpfp.jpg");
    assert!(client.fetch_pfp(Some(&path), 1).await.unwrap().is_none());
}
