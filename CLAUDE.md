# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Windowless Windows tray-less utility that lets a user adjust per-application audio session volume via global hotkeys, with feedback spoken through whichever screen reader / TTS backend is active. Inspired by an NVDA add-on (Python) at `C:\Users\Nitropc\Downloads\audiomanager` — the Rust port deliberately drops NVDA-specific pieces (microphone lock timer, recording-device nav, NVDA output-device nav, SessionNavigator's device-reassignment features).

## Build / run

```
cargo build            # debug
cargo build --release  # release, strips symbols + LTO
cargo run              # runs debug exe — headless, speaks "Audio manager ready"
```

Hotkeys (all use `Ctrl+Win+Alt` as the base modifier, registered with `MOD_NOREPEAT`):

- Left / Right — previous / next audio session
- Up / Down — volume ±2%
- PageUp / PageDown — volume +20% / -20%
- Home / End — set volume to 0% / 100%
- Q — quit

The app has no window and no console (`#![windows_subsystem = "windows"]`). Kill via the Quit hotkey or Task Manager.

## Architecture

Three modules, one binary:

- **`src/main.rs`** — owns the Win32 message loop. Registers hotkeys against `HWND(null)` so `WM_HOTKEY` posts to the thread queue. All volume operations funnel through `apply_volume()` which takes a closure over the current volume — `adjust()` and `set_absolute()` are thin wrappers. `State.refresh()` re-enumerates sessions on every Left/Right press and tries to preserve the current selection by session name.
- **`src/audio.rs`** — WASAPI session enumeration. Wraps `IAudioSessionControl2` + `ISimpleAudioVolume` for the default render endpoint. Process name comes from `QueryFullProcessImageNameW` with `PROCESS_QUERY_LIMITED_INFORMATION` (deliberately not `GetModuleBaseNameW` — the latter needs `PROCESS_VM_READ` and fails on protected processes). `pid == 0` identifies the system-sounds session; **do not** rely on `IsSystemSoundsSession().is_ok()` for that check — the method returns `S_OK`/`S_FALSE` (both COM-success) and `windows-rs` collapses both to `Ok(())`, so every session looks like system sounds.
- **`src/prism.rs`** — minimal FFI for prism TTS (header at `t:/code/prism/prism-windows-x64/include/prism.h`). `Speaker::new` does `prism_init` → `prism_registry_acquire_best` → `prism_backend_initialize`. `acquire_best` does *not* transfer ownership — `prism_shutdown(ctx)` cleans up acquired backends; do **not** call `prism_backend_free` on it. Auto-selects NVDA / JAWS / SAPI / whatever's running.

## Linking prism

`build.rs` hardcodes `t:/code/prism/prism-windows-x64/dynamic/release` for the import lib (`prism.lib`) and copies `prism.dll` next to the built exe on each build. The DLL must sit next to `audioman.exe` at runtime. If the prism path changes, edit `build.rs` — there is no env var override.

## Threading / COM

Main thread calls `CoInitializeEx(COINIT_APARTMENTTHREADED)` once at startup. All WASAPI / session enumeration runs on that thread. Don't introduce worker threads for audio work without also initializing COM on them.
