# Investigation: Hitman-Absolution does not start from the OpenSync Flatpak launcher

Date: 2026-11-25
Status: **root cause identified** (fix pending)

## Symptom

Clicking Play on `Hitman-Absolution` (server game id `3`) in the installed
OpenSync Launcher Flatpak (`io.opensync.launcher`, version `0.1.0`) never shows
a game window. The launcher's activity log falsely reports a short session:

```
game_start  Runtime: GE-Proton11-9-x86_64
game_close  Played 2s · Auto-syncing saves…
sync_complete  Saves up to date (10357 files verified)
```

The same game files launch fine through Lutris.

## Evidence

### The game files are not the problem

- Launcher install dir (from Flatpak `launcher.db` → `installed_games`):
  `/home/aki/Games/OpenSync/3e4385f1-f99b-4904-973d-92c0139a5c15/hitman-absolution`
- Saved executable override: `Hitman Absolution/retail/HMA.exe`
- `HMA.exe` is byte-identical (md5 `993d31201884c59294cdc4e31b1a75cb`) to the
  server copy at `/home/aki/OpenSync/storage/games/3/files/Hitman Absolution/retail/HMA.exe`.
- `file` reports: `PE32 executable for MS Windows 5.01 (GUI), Intel i386`
  → **this is a 32-bit Windows game**, so Wine needs a 32-bit (i386) runtime.

### The exact command the launcher builds works on the host

`opensync-proton::build_launch_command()` for a Windows PE on Linux:
- `umu-run` is **not** on `PATH` (neither on the host nor inside the Flatpak), so
  it falls back to `proton run <exe>` (no `(umu)` suffix in the logged runtime label
  confirms this).
- Env: `STEAM_COMPAT_DATA_PATH=<data>/prefixes/3`,
  `STEAM_COMPAT_CLIENT_INSTALL_PATH=~/.local/share/Steam/compatibilitytools.d`,
  `PROTONPATH=.../GE-Proton11-9-x86_64`, `GAMEID=opensync-0`,
  `STEAM_COMPAT_APP_ID=0`.

Running that exact command **from the host shell** with a 25 s timeout stayed
alive (timeout exit `124`) — i.e. the game launches.

### The same command fails inside the Flatpak

```
$ flatpak run --command=sh io.opensync.launcher -c '... proton run HMA.exe'
ProtonFixes[...] WARN: Skipping fix execution. We are probably running a unit test.
ntsync: up and running.
/lib/ld-linux.so.2: could not open       <-- fatal
EXIT=1
```

Inside the sandbox:

```
$ ls -la /lib/ld-linux.so.2
lrwxrwxrwx /lib/ld-linux.so.2 -> i386-linux-gnu/ld-linux.so.2   # dangling
$ ls /lib/i386-linux-gnu/
# (empty — zero 32-bit libraries)
```

### Why the Flatpak has no 32-bit libraries

`packaging/flatpak/io.opensync.launcher.yml` uses `org.gnome.Platform` **49**
and only declares `--allow=multiarch`. Multiarch permission merely *allows*
32-bit execution; it does not provide the libraries.

GNOME 49 **removed** the `org.gnome.Platform.i386.Compat` extension (see
https://blogs.gnome.org/alatiera/2025/10/13/flatpak-32bit/). The manifest does
not add the freedesktop `org.freedesktop.Platform.Compat.i386` extension
point, so the sandbox has no i386 loader/libs at all. `flatpak list --runtime`
does show `org.freedesktop.Platform.Compat.i386 25.08` installed, but GNOME
Platform 49's metadata does not expose that extension to this app.

Lutris works because it runs on the host, where `/lib32/ld-linux.so.2` and the
rest of the 32-bit glibc exist.

### Secondary bug: the failure is hidden

`opensync-runner::launch_and_track_with_hook()` ignores the child's exit code
(`on_process_exit(active_secs, status.code())`, but the hook in `launch_game`
discards it). A Proton process that dies after ~2 s is therefore recorded as a
normal short play session followed by a successful save sync — which is why the
activity log looked healthy while the game never appeared.

## Root cause

**The installed OpenSync Launcher Flatpak lacks a 32-bit (i386) runtime.
Hitman-Absolution's `HMA.exe` is a 32-bit PE, so Proton/Wine dies immediately
with `/lib/ld-linux.so.2: could not open`. The launcher ignores the non-zero
exit code and reports a fake short session.**

## Fix direction

1. Flatpak manifest: add the `org.freedesktop.Platform.Compat.i386` (and GL32)
   extension points and `LD_LIBRARY_PATH`, following the GNOME-49 port used by
   Bottles/Lutris. Requires rebuilding/reinstalling the Flatpak.
2. Launcher: surface a clear error when the child exits non-zero almost
   immediately (and when a Windows PE needs a 32-bit loader that is missing),
   instead of silently recording a success.
3. Optional robustness: prefer/require `umu-run` where available so Proton gets
   a complete runtime (Lutris already does this).
