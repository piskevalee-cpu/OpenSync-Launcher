#![allow(dead_code)]

use crc32fast::Hasher as Crc32;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const TEST_CHUNK_SIZE: usize = 64 * 1024; // 64 KB in tests so multi-chunk uploads are fast & deterministic

#[derive(Clone)]
pub struct MockGame {
    pub id: i64,
    pub name: String,
    pub slug: String,
    pub files: HashMap<String, Vec<u8>>,
    pub corrupt_download_file: Option<String>,
}

#[derive(Default)]
pub struct MockServerState {
    pub users: HashMap<String, (i64, String, String)>, // username -> (id, password, cookie)
    pub games: HashMap<i64, MockGame>,
    /// game_id -> (bytes, content_type)
    pub covers: HashMap<i64, (Vec<u8>, String)>,
    /// Public static routes (e.g. /releases/GE-Proton.tar.gz) -> (bytes, content_type)
    pub public_files: HashMap<String, (Vec<u8>, String)>,
    /// (user_id, game_id) -> rel_path -> bytes
    pub overlays: HashMap<(i64, i64), HashMap<String, Vec<u8>>>,
    /// (user_id, game_id) -> rel_path -> partial bytes
    pub parts: HashMap<(i64, i64), HashMap<String, Vec<u8>>>,
    /// (user_id, game_id) -> deleted paths
    pub deletions: HashMap<(i64, i64), Vec<String>>,
    pub sync_complete_calls: usize,
}

pub struct MockOpenSyncServer {
    pub addr: SocketAddr,
    pub base_url: String,
    pub state: Arc<Mutex<MockServerState>>,
    pub offline: Arc<AtomicBool>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl MockOpenSyncServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let state = Arc::new(Mutex::new(MockServerState::default()));
        let offline = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::oneshot::channel();

        let st = state.clone();
        let off = offline.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    Ok((stream, _)) = listener.accept() => {
                        let st = st.clone();
                        let off = off.clone();
                        tokio::spawn(async move {
                            let _ = handle_conn(stream, st, off).await;
                        });
                    }
                }
            }
        });

        Self {
            addr,
            base_url,
            state,
            offline,
            shutdown: tx,
        }
    }

    pub fn set_offline(&self, is_offline: bool) {
        self.offline.store(is_offline, Ordering::SeqCst);
    }

    pub fn add_user(&self, id: i64, username: &str, password: &str) -> String {
        let cookie = format!("ohs=tok-{username}-{id}");
        self.state.lock().unwrap().users.insert(
            username.to_string(),
            (id, password.to_string(), cookie.clone()),
        );
        cookie
    }

    pub fn add_game(&self, id: i64, name: &str, slug: &str, files: Vec<(&str, Vec<u8>)>) {
        let map: HashMap<String, Vec<u8>> = files
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        self.state.lock().unwrap().games.insert(
            id,
            MockGame {
                id,
                name: name.to_string(),
                slug: slug.to_string(),
                files: map,
                corrupt_download_file: None,
            },
        );
    }

    pub fn set_corrupt_download(&self, game_id: i64, rel_path: &str) {
        if let Some(g) = self.state.lock().unwrap().games.get_mut(&game_id) {
            g.corrupt_download_file = Some(rel_path.to_string());
        }
    }

    pub fn set_cover(&self, game_id: i64, bytes: Vec<u8>, content_type: &str) {
        self.state
            .lock()
            .unwrap()
            .covers
            .insert(game_id, (bytes, content_type.to_string()));
    }

    pub fn add_public_file(&self, path: &str, bytes: Vec<u8>, content_type: &str) {
        self.state
            .lock()
            .unwrap()
            .public_files
            .insert(path.to_string(), (bytes, content_type.to_string()));
    }

    pub fn overlay_files(&self, user_id: i64, game_id: i64) -> HashMap<String, Vec<u8>> {
        self.state
            .lock()
            .unwrap()
            .overlays
            .get(&(user_id, game_id))
            .cloned()
            .unwrap_or_default()
    }

    pub fn overlay_deletions(&self, user_id: i64, game_id: i64) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .deletions
            .get(&(user_id, game_id))
            .cloned()
            .unwrap_or_default()
    }

    pub fn sync_count(&self) -> usize {
        self.state.lock().unwrap().sync_complete_calls
    }
}

impl Drop for MockOpenSyncServer {
    fn drop(&mut self) {
        let (dummy_tx, _) = tokio::sync::oneshot::channel();
        let tx = std::mem::replace(&mut self.shutdown, dummy_tx);
        let _ = tx.send(());
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    state: Arc<Mutex<MockServerState>>,
    offline: Arc<AtomicBool>,
) -> std::io::Result<()> {
    if offline.load(Ordering::SeqCst) {
        // Immediately drop TCP connection to simulate unreachable/crashed server
        drop(stream);
        return Ok(());
    }

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end;
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
    }

    let header_text = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let mut lines = header_text.lines();
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let full_target = parts.next().unwrap_or("/").to_string();

    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let content_len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body = raw[header_end..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    body.truncate(content_len);

    if offline.load(Ordering::SeqCst) {
        drop(stream);
        return Ok(());
    }

    let (path, query) = full_target
        .split_once('?')
        .map(|(p, q)| (p.to_string(), q.to_string()))
        .unwrap_or((full_target, String::new()));

    let (status, ct, extra_headers, resp_body) =
        route(&method, &path, &query, &headers, &body, &state);

    let mut resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp_body.len()
    );
    for (k, v) in extra_headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str("\r\n");
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(&resp_body).await?;
    stream.flush().await?;
    Ok(())
}

fn route(
    method: &str,
    path: &str,
    query: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
    state: &Arc<Mutex<MockServerState>>,
) -> (
    &'static str,
    String,
    Vec<(&'static str, String)>,
    Vec<u8>,
) {
    let json_resp = |status: &'static str, val: serde_json::Value| {
        (
            status,
            "application/json".to_string(),
            vec![],
            serde_json::to_vec(&val).unwrap(),
        )
    };

    if method == "GET"
        && let Some((bytes, ct)) = state.lock().unwrap().public_files.get(path).cloned()
    {
        return ("200 OK", ct, vec![], bytes);
    }

    if method == "GET" && path == "/api/health" {
        return json_resp("200 OK", json!({ "ok": true, "name": "opensync" }));
    }
    if method == "GET" && path == "/api/info" {
        let has_admin = !state.lock().unwrap().users.is_empty();
        return json_resp(
            "200 OK",
            json!({ "name": "opensync", "has_admin": has_admin }),
        );
    }

    if method == "POST" && (path == "/api/auth/login" || path == "/api/auth/register") {
        let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        let u = parsed["username"].as_str().unwrap_or("").trim().to_string();
        let p = parsed["password"].as_str().unwrap_or("").to_string();
        let mut st = state.lock().unwrap();
        if path == "/api/auth/register" {
            let id = (st.users.len() as i64) + 1;
            let cookie = format!("ohs=tok-{u}-{id}");
            st.users.insert(u.clone(), (id, p, cookie.clone()));
            return (
                "201 Created",
                "application/json".to_string(),
                vec![("Set-Cookie", format!("{cookie}; Path=/; HttpOnly"))],
                serde_json::to_vec(&json!({
                    "user": { "id": id, "username": u, "role": if id == 1 { "admin" } else { "user" } }
                }))
                .unwrap(),
            );
        }
        if let Some((id, pass, cookie)) = st.users.get(&u)
            && *pass == p
        {
            return (
                "200 OK",
                "application/json".to_string(),
                vec![("Set-Cookie", format!("{cookie}; Path=/; HttpOnly"))],
                serde_json::to_vec(&json!({
                    "user": { "id": *id, "username": u, "role": if *id == 1 { "admin" } else { "user" } }
                }))
                .unwrap(),
            );
        }
        return json_resp("401 Unauthorized", json!({ "error": "invalid credentials" }));
    }

    // Authenticated routes below
    let cookie = headers.get("cookie").cloned().unwrap_or_default();
    let authed_user = {
        let st = state.lock().unwrap();
        st.users
            .iter()
            .find(|(_, (_, _, c))| cookie.contains(c.as_str()))
            .map(|(name, (id, _, _))| (*id, name.clone()))
    };
    let Some((user_id, username)) = authed_user else {
        return json_resp("401 Unauthorized", json!({ "error": "unauthorized" }));
    };

    if method == "GET" && path == "/api/auth/me" {
        return json_resp(
            "200 OK",
            json!({
                "user": {
                    "id": user_id,
                    "username": username,
                    "role": if user_id == 1 { "admin" } else { "user" },
                }
            }),
        );
    }

    if method == "GET" && path == "/api/games" {
        let st = state.lock().unwrap();
        let games: Vec<serde_json::Value> = st
            .games
            .values()
            .map(|g| {
                let total_size: u64 = g.files.values().map(|v| v.len() as u64).sum();
                let has_overlay = st
                    .overlays
                    .get(&(user_id, g.id))
                    .is_some_and(|m| !m.is_empty());
                json!({
                    "id": g.id,
                    "name": g.name,
                    "slug": g.slug,
                    "status": "ready",
                    "total_size": total_size,
                    "has_overlay": has_overlay,
                    "download_count": 0,
                    "created_at": "2026-01-01T00:00:00Z",
                })
            })
            .collect();
        return json_resp("200 OK", json!({ "games": games }));
    }

    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segs.len() >= 3 && segs[0] == "api" && segs[1] == "games" {
        let Ok(game_id) = segs[2].parse::<i64>() else {
            return json_resp("404 Not Found", json!({ "error": "game not found" }));
        };

        if method == "GET" && segs.len() == 3 {
            let st = state.lock().unwrap();
            let Some(g) = st.games.get(&game_id) else {
                return json_resp("404 Not Found", json!({ "error": "game not found" }));
            };
            let total_size: u64 = g.files.values().map(|v| v.len() as u64).sum();
            let ov = st.overlays.get(&(user_id, game_id));
            let dels = st.deletions.get(&(user_id, game_id));
            let ov_files = ov.map(|m| m.len()).unwrap_or(0);
            let ov_size: u64 = ov
                .map(|m| m.values().map(|v| v.len() as u64).sum())
                .unwrap_or(0);
            let del_count = dels.map(|d| d.len()).unwrap_or(0);
            return json_resp(
                "200 OK",
                json!({
                    "game": {
                        "id": g.id,
                        "name": g.name,
                        "slug": g.slug,
                        "status": "ready",
                        "total_size": total_size,
                        "has_overlay": ov_files > 0 || del_count > 0,
                        "download_count": 0,
                        "created_at": "2026-01-01T00:00:00Z",
                    },
                    "overlay": {
                        "has_overlay": ov_files > 0 || del_count > 0,
                        "files": ov_files,
                        "deletions": del_count,
                        "size": ov_size,
                        "updated_at": null
                    }
                }),
            );
        }

        if method == "GET" && segs.len() == 4 && segs[3] == "cover" {
            let st = state.lock().unwrap();
            let Some((bytes, ct)) = st.covers.get(&game_id).cloned() else {
                return json_resp("404 Not Found", json!({ "error": "cover not found" }));
            };
            return ("200 OK", ct, vec![], bytes);
        }

        if method == "GET" && segs.len() == 4 && segs[3] == "manifest" {
            let st = state.lock().unwrap();
            let Some(g) = st.games.get(&game_id) else {
                return json_resp("404 Not Found", json!({ "error": "game not found" }));
            };
            let mut entries: Vec<serde_json::Value> = g
                .files
                .iter()
                .map(|(p, data)| {
                    json!({
                        "path": p,
                        "hash": hex::encode(Sha256::digest(data)),
                        "size": data.len(),
                    })
                })
                .collect();
            entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
            return json_resp("200 OK", json!({ "game_id": game_id, "files": entries }));
        }

        if method == "GET" && segs.len() == 4 && segs[3] == "download" {
            let st = state.lock().unwrap();
            let Some(g) = st.games.get(&game_id) else {
                return json_resp("404 Not Found", json!({ "error": "game not found" }));
            };
            let fresh = query.contains("fresh=1") || query.contains("fresh=true");
            let mut merged = g.files.clone();
            if !fresh {
                if let Some(dels) = st.deletions.get(&(user_id, game_id)) {
                    for d in dels {
                        merged.remove(d);
                    }
                }
                if let Some(ov) = st.overlays.get(&(user_id, game_id)) {
                    for (k, v) in ov {
                        merged.insert(k.clone(), v.clone());
                    }
                }
            }
            if let Some(bad) = &g.corrupt_download_file
                && let Some(bytes) = merged.get_mut(bad)
            {
                bytes.push(0xFF);
            }
            let mut sorted: Vec<(String, Vec<u8>)> = merged.into_iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let zip_bytes = build_archiver_store_zip(&sorted);
            return ("200 OK", "application/zip".to_string(), vec![], zip_bytes);
        }

        if method == "GET" && segs.len() == 5 && segs[3] == "overlay" && segs[4] == "init" {
            let st = state.lock().unwrap();
            let files: Vec<serde_json::Value> = st
                .parts
                .get(&(user_id, game_id))
                .map(|m| {
                    m.iter()
                        .map(|(p, data)| {
                            json!({ "path": p, "received": data.len() / TEST_CHUNK_SIZE })
                        })
                        .collect()
                })
                .unwrap_or_default();
            return json_resp(
                "200 OK",
                json!({ "chunkSize": TEST_CHUNK_SIZE, "files": files }),
            );
        }

        if method == "POST" && segs.len() == 5 && segs[3] == "overlay" && segs[4] == "files" {
            let rel_path = urlencoding::decode(headers.get("x-path").map(String::as_str).unwrap_or(""))
                .unwrap_or_default()
                .to_string();
            let index: usize = headers
                .get("x-index")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let total: usize = headers
                .get("x-total")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let declared_size: usize = headers
                .get("x-size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let declared_hash = headers.get("x-hash").cloned().unwrap_or_default();

            let mut st = state.lock().unwrap();
            let part_entry = st
                .parts
                .entry((user_id, game_id))
                .or_default()
                .entry(rel_path.clone())
                .or_default();
            let expected_offset = index * TEST_CHUNK_SIZE;
            if part_entry.len() != expected_offset {
                let received = part_entry.len() / TEST_CHUNK_SIZE;
                return json_resp(
                    "409 Conflict",
                    json!({ "error": "chunk out of order", "received": received }),
                );
            }
            part_entry.extend_from_slice(body);
            if index + 1 == total {
                let full = st
                    .parts
                    .entry((user_id, game_id))
                    .or_default()
                    .remove(&rel_path)
                    .unwrap_or_default();
                if full.len() != declared_size {
                    return json_resp("400 Bad Request", json!({ "error": "size mismatch" }));
                }
                let actual_hash = hex::encode(Sha256::digest(&full));
                if !declared_hash.is_empty() && actual_hash != declared_hash {
                    return json_resp(
                        "422 Unprocessable Entity",
                        json!({ "error": "hash mismatch" }),
                    );
                }
                st.overlays
                    .entry((user_id, game_id))
                    .or_default()
                    .insert(rel_path, full);
                return json_resp("200 OK", json!({ "ok": true, "done": true }));
            }
            return json_resp(
                "200 OK",
                json!({ "ok": true, "done": false, "received": index + 1 }),
            );
        }

        if method == "POST" && segs.len() == 5 && segs[3] == "sync" && segs[4] == "complete" {
            let parsed: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
            let dels: Vec<String> = parsed["deletions"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let force = parsed["force"].as_bool().unwrap_or(false);
            let mut st = state.lock().unwrap();
            let clean_count = st
                .games
                .get(&game_id)
                .map(|g| g.files.len())
                .unwrap_or(0);
            if !force && clean_count > 0 && dels.len() * 2 > clean_count {
                return json_resp("400 Bad Request", json!({ "error": "mass deletion guard" }));
            }
            st.deletions.insert((user_id, game_id), dels);
            st.sync_complete_calls += 1;
            return json_resp("200 OK", json!({ "ok": true }));
        }
    }

    json_resp("404 Not Found", json!({ "error": "not found" }))
}

/// Emits a bit-3 Data Descriptor STORE ZIP identical to Node's `archiver` v8.
fn build_archiver_store_zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0008u16.to_le_bytes()); // data descriptor
        out.extend_from_slice(&0u16.to_le_bytes()); // STORE
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        let mut crc = Crc32::new();
        crc.update(data);
        out.extend_from_slice(&[0x50, 0x4b, 0x07, 0x08]);
        out.extend_from_slice(&crc.finalize().to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    }
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out
}
