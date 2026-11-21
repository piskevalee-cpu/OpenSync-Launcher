# OpenSync Launcher

Native desktop game cabinet and runner for self-hosted [OpenSync](https://github.com/piskevalee-cpu/OpenSync) archives, built with **Rust** and **Tauri v2**.

## Highlights

- **Multi-profile connection manager** — remembers server profiles, sessions, and downloaded games per profile across restarts, with exponential-backoff auto-reconnect.
- **High-speed streaming downloads** — zero-temp-file streaming ZIP extraction with 1 MB buffered I/O, inline SHA-256 verification, path-traversal defense, pause/resume/cancel, bandwidth throttling, and live rolling throughput metrics.
- **Automatic per-session save sync** — fast parallel `rayon` SHA-256 diffing against the clean manifest and concurrent 4 MB chunked overlay upload triggered automatically whenever a game closes (with offline pending-sync queue flushed on reconnect).
- **Proton auto-detection & download** — detects OpenSync-managed, Steam `compatibilitytools.d`, Steam `common`, and Flatpak Steam Proton installs; offers one-click SHA-512 verified GE-Proton downloads streamed through the Download Manager.
- **Screen-time tracking** — tracks per-session and cumulative playtime per game with system sleep/suspend exclusion.
- **Tauri v2 desktop UI** — matches OpenSync Web's dark aesthetic (`#0a0a0c` / `#14141a` / `#7dfa9b`), universal `2:3` Steam capsule covers with 3D perspective tilt + fake-glass sheen, circular frosted glass Play/Download buttons, frosted glass footers, and a 60fps canvas Download Manager waveform.

## Workspace Crates

| Crate | Purpose |
|---|---|
| `crates/launcher-db` | Bundled SQLite store (profiles, remembered state, installed games, play sessions, offline sync queue) |
| `crates/opensync-api` | Async HTTP client for OpenSync v1.0 endpoints (`reqwest` + `rustls`, tuned TCP/pool settings) |
| `crates/opensync-transfer` | Streaming STORE/Deflate ZIP extractor, CRC32 + Data-Descriptor support, inline SHA-256 verifier, speed tracker |
| `crates/opensync-sync` | Parallel `rayon` folder scanner, clean-manifest differ, concurrent 4 MB chunked overlay uploader |
| `crates/opensync-proton` | ELF/PE binary detection, executable discovery, Proton scanner, streaming GE-Proton installer & prefix runner |
| `crates/opensync-runner` | Process launcher, sleep-aware screen-time clock, and automatic post-exit save sync orchestration |
| `crates/launcher-core` | Stateful `LauncherCore` facade (auto-reconnect, profile switching/remembrance, download manager, cover cache) |
| `crates/launcher-tauri` | Tauri v2 desktop shell (`src/commands.rs`, `src/main.rs`) and webview frontend (`ui/`) |

## Running Tests

```bash
cd launcher
cargo test
```

## Building & Running the Tauri v2 Desktop App (Linux)

On **openSUSE Tumbleweed**, install the WebKitGTK & GTK3 development packages required by Tauri v2:

```bash
sudo zypper in webkitgtk3-devel gtk3-devel libsoup-devel librsvg-devel
cargo run -p launcher-tauri
```

On **Ubuntu / Debian**:

```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libsoup-3.0-dev librsvg2-dev patchelf
cargo run -p launcher-tauri
```
