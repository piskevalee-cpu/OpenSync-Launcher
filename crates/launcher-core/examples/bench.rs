#[path = "../tests/common/mod.rs"]
mod common;

use common::MockOpenSyncServer;
use launcher_core::{LauncherCore, LauncherPaths};
use launcher_db::{FileSecretStore, LauncherDb, MemorySecretStore, SecretStore};
use opensync_transfer::DownloadControl;
use std::fs;
use std::time::Instant;
use tempfile::tempdir;

fn ms(elapsed: std::time::Duration) -> f64 {
    elapsed.as_secs_f64() * 1000.0
}

fn mb_per_sec(bytes: usize, elapsed: std::time::Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        0.0
    } else {
        (bytes as f64 / (1024.0 * 1024.0)) / secs
    }
}

#[tokio::main]
async fn main() {
    let total_start = Instant::now();

    // 1. db_and_secrets: LauncherDb::open + schema init + 200 DB queries + 500 FileSecretStore::get calls
    {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("bench.db");
        let secrets_path = dir.path().join("secrets.json");

        let t0 = Instant::now();
        let db = LauncherDb::open(&db_path).unwrap();
        let secrets = FileSecretStore::new(&secrets_path);
        secrets.set("bench:token", "secret-cookie-value").unwrap();
        let profile = db
            .upsert_profile("Bench", "http://127.0.0.1:3000", "valerio", 1, "user")
            .unwrap();

        for _ in 0..200 {
            let _ = db.list_installed_games(&profile.id).unwrap();
            let _ = db.playtime_summaries_for_profile(&profile.id).unwrap();
        }
        for _ in 0..500 {
            let _ = secrets.get("bench:token").unwrap();
        }
        let elapsed = t0.elapsed();
        println!(
            "db_and_secrets: {:.2} ms (open+init + 200x2 DB queries + 500 FileSecretStore::get)",
            ms(elapsed)
        );
    }

    // 2. detect_proton_5x: core.detect_proton() called 5 times in a row
    {
        let dir = tempdir().unwrap();
        let db = LauncherDb::open_in_memory().unwrap();
        let secrets = MemorySecretStore::new();
        let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

        let t0 = Instant::now();
        let mut found = 0usize;
        for _ in 0..5 {
            found = core.detect_proton().len();
        }
        let elapsed = t0.elapsed();
        println!(
            "detect_proton_5x: {:.2} ms (5 calls, {} installations found)",
            ms(elapsed),
            found
        );
    }

    // 3. discover_executables_15: opensync_proton::discover_executables across 15 simulated game dirs
    {
        let dir = tempdir().unwrap();
        let mut game_dirs = Vec::with_capacity(15);
        for i in 0..15 {
            let gdir = dir.path().join(format!("game_{i}"));
            fs::create_dir_all(gdir.join("bin")).unwrap();
            for j in 0..10 {
                fs::write(gdir.join(format!("asset_{j}.dat")), b"data").unwrap();
            }
            // 1 Windows PE (.exe) + 1 Linux ELF
            fs::write(gdir.join(format!("Game{i}.exe")), b"MZ\x90\x00").unwrap();
            fs::write(gdir.join(format!("bin/game_{i}")), b"\x7fELF\x02\x01\x01\x00").unwrap();
            game_dirs.push(gdir);
        }

        let t0 = Instant::now();
        let mut total_exes = 0usize;
        for (i, gdir) in game_dirs.iter().enumerate() {
            total_exes += opensync_proton::discover_executables(gdir, &format!("Game{i}")).len();
        }
        let elapsed = t0.elapsed();
        println!(
            "discover_executables_15: {:.2} ms (15 dirs x 12 files, {} executables found)",
            ms(elapsed),
            total_exes
        );
    }

    // 4. idle_background_sync_8x4mb: 8 games x 4 MB = 32 MB, 2 consecutive passes with 0 local edits
    {
        let dir = tempdir().unwrap();
        let db = LauncherDb::open_in_memory().unwrap();
        let secrets = MemorySecretStore::new();
        let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

        let srv = MockOpenSyncServer::start().await;
        srv.add_user(1, "valerio", "secret123");

        let payload_4mb = vec![0x42u8; 4 * 1024 * 1024];
        for id in 1..=8 {
            srv.add_game(
                id,
                &format!("IdleGame{id}"),
                &format!("idle-game-{id}"),
                vec![("data/pak0.bin", payload_4mb.clone())],
            );
        }

        core.connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
            .await
            .unwrap();
        for id in 1..=8 {
            core.download_game(id, true, DownloadControl::new(0, 0))
                .await
                .unwrap();
        }

        let t1 = Instant::now();
        let out1 = core.run_background_auto_sync_once().await.unwrap();
        let pass1 = t1.elapsed();

        let t2 = Instant::now();
        let out2 = core.run_background_auto_sync_once().await.unwrap();
        let pass2 = t2.elapsed();

        println!(
            "idle_background_sync_8x4mb: pass1 = {:.2} ms (synced={}), pass2 = {:.2} ms (synced={})",
            ms(pass1),
            out1.synced_games,
            ms(pass2),
            out2.synced_games
        );
    }

    // 5. diff_sync_16mb: sync_game_now on a 16 MB game with 1 modified 64 KB save file
    {
        let dir = tempdir().unwrap();
        let db = LauncherDb::open_in_memory().unwrap();
        let secrets = MemorySecretStore::new();
        let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

        let srv = MockOpenSyncServer::start().await;
        srv.add_user(1, "valerio", "secret123");
        let big_bin = vec![0x11u8; 16 * 1024 * 1024 - 64 * 1024];
        let save_64kb = vec![0x22u8; 64 * 1024];
        srv.add_game(
            100,
            "DiffSyncGame",
            "diff-sync-game",
            vec![
                ("game.pak", big_bin),
                ("saves/slot1.sav", save_64kb),
            ],
        );

        core.connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
            .await
            .unwrap();
        let (installed, _) = core
            .download_game(100, true, DownloadControl::new(0, 0))
            .await
            .unwrap();

        let modified_save = vec![0x99u8; 64 * 1024];
        fs::write(installed.install_dir.join("saves/slot1.sav"), &modified_save).unwrap();

        let t0 = Instant::now();
        let report = core.sync_game_now(100, false).await.unwrap();
        let elapsed = t0.elapsed();
        println!(
            "diff_sync_16mb: {:.2} ms ({:.2} MB/s scan+upload, uploaded={}, skipped={})",
            ms(elapsed),
            mb_per_sec(16 * 1024 * 1024, elapsed),
            report.uploaded_files,
            report.skipped_unchanged
        );
    }

    // 6. differential_download_16mb: download_game(id, false) vs download_game(id, true) on 16 MB game
    {
        let dir = tempdir().unwrap();
        let db = LauncherDb::open_in_memory().unwrap();
        let secrets = MemorySecretStore::new();
        let core = LauncherCore::new(db, secrets, LauncherPaths::under(dir.path()));

        let srv = MockOpenSyncServer::start().await;
        srv.add_user(1, "valerio", "secret123");
        let big_bin = vec![0x55u8; 16 * 1024 * 1024 - 64 * 1024];
        let save_64kb = vec![0x66u8; 64 * 1024];
        srv.add_game(
            200,
            "DiffDlGame",
            "diff-dl-game",
            vec![
                ("game.pak", big_bin),
                ("saves/slot1.sav", save_64kb),
            ],
        );
        // Seed a 64 KB overlay save on the server for user 1, game 200
        {
            let mut st = srv.state.lock().unwrap();
            st.overlays
                .entry((1, 200))
                .or_default()
                .insert("saves/slot1.sav".to_string(), vec![0xAAu8; 64 * 1024]);
        }

        core.connect_with_password("LAN", &srv.base_url, "valerio", "secret123")
            .await
            .unwrap();

        let t_overlay = Instant::now();
        let (_, out_overlay) = core
            .download_game(200, false, DownloadControl::new(0, 0))
            .await
            .unwrap();
        let elapsed_overlay = t_overlay.elapsed();

        let t_clean = Instant::now();
        let (_, out_clean) = core
            .download_game(200, true, DownloadControl::new(0, 0))
            .await
            .unwrap();
        let elapsed_clean = t_clean.elapsed();

        println!(
            "differential_download_16mb: with_overlay = {:.2} ms ({:.2} MB/s, {} B), clean = {:.2} ms ({:.2} MB/s, {} B)",
            ms(elapsed_overlay),
            mb_per_sec(out_overlay.total_bytes as usize, elapsed_overlay),
            out_overlay.total_bytes,
            ms(elapsed_clean),
            mb_per_sec(out_clean.total_bytes as usize, elapsed_clean),
            out_clean.total_bytes
        );
    }

    println!("total_bench_time: {:.2} ms", ms(total_start.elapsed()));
}
