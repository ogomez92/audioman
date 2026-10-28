# audioman

A tiny Windows utility for controlling per-application audio volume with global hotkeys. It runs headless (no window, no tray icon, no console) and announces what you're doing through your screen reader or the system speech voice — so you can change an app's volume from anywhere without looking.

Works with NVDA, JAWS, Narrator, or falls back to SAPI if no screen reader is running. Speech is handled by [prism](https://github.com/prism-tts/prism).

## Install

1. Download the latest release (`audioman.exe` + `prism.dll`).
2. Put both files in the **same folder** — the DLL must sit next to the exe.
3. Double-click `audioman.exe`. You should hear "Audio manager ready."
4. (Optional) drop a shortcut in `shell:startup` so it launches at login.

To quit, press the quit hotkey below or kill it from Task Manager. There is nothing visible to click.

## Hotkeys

All hotkeys use **Ctrl + Win + Alt** as the modifier.

| Key | Action |
|---|---|
| Left / Right | Previous / next audio app |
| Up / Down | Volume ±2% |
| Page Up / Page Down | Volume +20% / -20% |
| Home / End | Set volume to 0% / 100% |
| M | Toggle mute |
| Enter | Choose input/output devices for the selected app (device mode: enable/disable device) |
| 0 | Switch between application mode and device mode |
| Shift + 0 | Choose which applications (application mode) or devices (device mode) Left/Right cycle through |
| Shift + Enter | Duplicate everything playing on one output device to another; press again to stop |
| Shift + S | Stream an output device (everything playing on it) or a microphone into a SonicRoom room; press again to stop |
| Backspace | Reset every app in the volume mixer to 100% and unmute |
| Q | Quit |

Streaming to SonicRoom asks for the server (or just paste a room link), the room, the name to show in the room, the device, mono or stereo (automatic: stereo for outputs, mono for microphones) and the quality: 5 ms packets for the lowest delay, 10 ms (the default), or 20 ms at a higher bitrate for the best quality. audioman joins as a music caster, so everyone in the room hears it and anyone there can remove it. Like duplication, a stream keeps going across restarts of audioman and resumes by itself after a lost connection or an unplugged device, until you stop it with Shift + S.

Left / Right cycle through whatever's currently in the Windows volume mixer — including system sounds and any app that's opened an audio stream. The list updates live as apps start and stop playing; you don't need to refresh it.

## What it can and can't do

**Can:** adjust per-session volume and mute for anything that shows up in the Windows volume mixer. The reset-all hotkey touches every mixer entry, not just the ones you've touched in audioman.

**Can't:** change which playback or recording device a specific app uses. That requires undocumented Windows APIs this tool deliberately avoids.

## Building from source

Requires Rust (stable) and the prism SDK for Windows x64. `build.rs` expects prism at `t:/code/prism/prism-windows-x64/dynamic/release`; edit the path there if yours lives elsewhere.

```
cargo build --release
```

The exe lands in `target/release/` along with `prism.dll` (copied by the build script). Both need to travel together.

## Acknowledgements

Inspired by the [NVDA audioManager add-on](https://github.com/CoolShenzi/audioManager) — this is a Rust reimplementation focused only on per-app volume, with live session notifications instead of polling.
