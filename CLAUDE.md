# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Windowless Windows tray-less utility that lets a user adjust per-application audio session volume, route individual apps to specific input/output devices, and manage endpoint devices (volume, mute, enable/disable) via global hotkeys, with feedback spoken through whichever screen reader / TTS backend is active. Inspired by an NVDA add-on (Python) at `C:\Users\Nitropc\Downloads\audiomanager` — the Rust port deliberately drops NVDA-specific pieces (microphone lock timer, NVDA output-device nav).

## Build / run

```
cargo build            # debug
cargo build --release  # release, strips symbols + LTO
cargo run              # runs debug exe — headless, speaks "Audio manager ready"
```

Hotkeys (all use `Ctrl+Win+Alt` as the base modifier, registered with `MOD_NOREPEAT`). `0` toggles between **application mode** (default) and **device mode**; the other keys act on whatever the mode targets:

- 0 — toggle application mode / device mode
- Left / Right — previous / next audio session (app mode) or audio device (device mode; outputs first, then inputs, including disabled devices; announces name, direction, enabled/disabled, mute, volume)
- Up / Down — volume ±2% (session or endpoint master volume by mode)
- PageUp / PageDown — volume +20% / -20%
- Home / End — set volume to 0% / 100%
- M — toggle mute (session or endpoint by mode)
- Backspace — restore every session to 100% and unmute (both modes)
- Enter — app mode: open the device picker to route **the selected app's** audio (input and output) to chosen devices; device mode: enable/disable the selected device
- Q — quit

The app has no window and no console (`#![windows_subsystem = "windows"]`). Kill via the Quit hotkey or Task Manager.

## Architecture

Six modules, one binary:

- **`src/main.rs`** — owns the Win32 message loop and the `Mode` (apps vs devices) state. Registers hotkeys against `HWND(null)` so `WM_HOTKEY` posts to the thread queue. Session volume ops funnel through `apply_volume()`, endpoint volume ops through `apply_device_volume()` — both take a closure over the current volume. Device-mode navigation (`DeviceNav`) re-enumerates on every Left/Right press and keeps selection by endpoint id. The loop also handles `audio::WM_DEVICES_CHANGED` (thread message, `WM_APP + 1`): it drains duplicate messages with `PeekMessageW` and calls `AudioManager::rebuild()`. After the picker closes it rebuilds unconditionally — thread messages posted while the dialog's nested loop ran are dropped.
- **`src/audio.rs`** — WASAPI session tracking across **all active render endpoints** (one `IAudioSessionManager2` + `IAudioSessionNotification` per device), not just the default — with per-app routing an app can live on any endpoint. An `IMMNotificationClient` posts `WM_DEVICES_CHANGED` to the main thread on any device add/remove/state/default change; the main thread responds with `rebuild()` (detach everything, re-enumerate, restore selection by pid then name). `add_session` **replaces** an existing same-pid session instead of skipping: when an app moves endpoints the new session appears before the old one expires, and skip-dedup would drop the app entirely when the old session died. Never call `UnregisterAudioSessionNotification` from inside a WASAPI callback (deadlock risk) — callback-path removals just drop refs; explicit unregistration happens only in `detach_all()` on the main thread. Process name comes from `QueryFullProcessImageNameW` with `PROCESS_QUERY_LIMITED_INFORMATION` (deliberately not `GetModuleBaseNameW` — the latter needs `PROCESS_VM_READ` and fails on protected processes). `pid == 0` identifies the system-sounds session; **do not** rely on `IsSystemSoundsSession().is_ok()` for that check — the method returns `S_OK`/`S_FALSE` (both COM-success) and `windows-rs` collapses both to `Ok(())`, so every session looks like system sounds.
- **`src/routing.rs`** — per-app device routing via the undocumented `Windows.Media.Internal.AudioPolicyConfig` WinRT class (what Settings → App volume and device preferences uses). `SetPersistedDefaultAudioEndpoint(pid, flow, role, path)` persists by app identity (survives restarts/reboots); a null HSTRING clears it. The endpoint argument is `\\?\SWD#MMDEVAPI#<id>#{DEVINTERFACE_AUDIO_RENDER-or-CAPTURE}`, not a bare MMDevice id. The factory IID changed at Win10 build 21390 (layout identical) — current IID first, then raw-QI fallback to the old one. The interface is IInspectable-based: 3 IInspectable methods + 19 stub slots precede the real methods — **do not reorder or remove them**. Set/get fail with `E_INVALIDARG` for pids without a live audio session, so only pass pids taken from enumerated sessions. Sets console + multimedia roles (matches Windows Settings).
- **`src/prism.rs`** — minimal FFI for prism TTS (header bundled at `vendor/prism/prism.h`). `Speaker::new` does `prism_init` → `prism_registry_acquire_best` → `prism_backend_initialize`. `acquire_best` does *not* transfer ownership — `prism_shutdown(ctx)` cleans up acquired backends; do **not** call `prism_backend_free` on it. Auto-selects NVDA / JAWS / SAPI / whatever's running.
- **`src/devices.rs`** — endpoint enumeration (`IMMDeviceEnumerator::EnumAudioEndpoints`; friendly name via `IPropertyStore` + `PKEY_Device_FriendlyName`), endpoint master volume/mute (`IAudioEndpointVolume`), default-device switching, and endpoint enable/disable. `all_endpoints()` enumerates `ACTIVE | DISABLED` so disabled devices stay reachable for re-enabling; `inputs()`/`outputs()` (used by the picker) stay active-only. Default switching and visibility have **no** public API — both go through the undocumented `IPolicyConfig` interface on `CPolicyConfigClient`, declared with `#[interface]`. Only `SetDefaultEndpoint` and `SetEndpointVisibility` are called; the earlier vtable slots are stubs that exist only to land them at the right offsets, so **do not reorder or remove them**. `set_default` sets all three roles (console / multimedia / communications).
- **`src/dialog.rs`** — the Enter-hotkey device picker, built from raw Win32 controls (`COMBOBOX` + `BUTTON` + `STATIC`) so it inherits native MSAA/UIA accessibility for NVDA/JAWS/Narrator. Each combo is named by the `STATIC` label created immediately before it (z-order label-association heuristic — **keep label-then-combo creation order**). Each combo's first item is a synthetic **"Default"** entry — real devices follow at combo index 1+, so device N sits at index N+1. The dialog is per-app: the title names the selected app, the caller preselects the app's current routing, and accepting returns a `Choice` per direction — `Default` means "follow the system default" (main clears the per-app assignment). For the pid-0 system-sounds session (no process to route), main falls back to switching the **system** default devices. It's a normal registered-class window (not a real dialog), so keyboard behavior is supplied manually: `IsDialogMessageW` for Tab/Shift+Tab, plus explicit Enter = accept / Esc = cancel that defers to an open combo dropdown via `CB_GETDROPPEDSTATE`. Runs a nested modal message loop on the main thread; `DialogState` lives on the heap with its pointer in `GWLP_USERDATA`. The screen reader narrates the controls natively, so the dialog itself does **not** drive prism — main speaks only the post-accept confirmation.

## Linking prism

prism is **vendored in-repo** at `vendor/prism/` (`prism.lib` — the x64 dynamic import library, `prism.dll`, and `prism.h`) so a fresh clone builds with no external SDK. `build.rs` resolves that directory via `CARGO_MANIFEST_DIR`, links `prism.lib`, and copies `prism.dll` next to the built exe on each build. The DLL must sit next to `audioman.exe` at runtime (`install.ps1` carries it across). To update prism, drop new files into `vendor/prism/` — they came from the SDK's `prism-windows-x64/{dynamic/release/lib,dynamic/release/bin,include}`. `build.rs` also declares `rerun-if-env-changed=CARGO_MANIFEST_DIR`: the link-search path is absolute, and without it a moved checkout keeps linking against the old location's cached path (LNK1181).

## Threading / COM

Main thread calls `CoInitializeEx(COINIT_APARTMENTTHREADED)` once at startup; `RoGetActivationFactory` (routing) rides on that same init. All WASAPI / session enumeration runs on that thread. Don't introduce worker threads for audio work without also initializing COM on them. WASAPI/MMDevice **callbacks** (`IAudioSessionNotification`, `IAudioSessionEvents`, `IMMNotificationClient`) arrive on WASAPI worker threads — they only touch the `Arc<Mutex<State>>` or `PostThreadMessageW`; keep COM object creation out of them.
