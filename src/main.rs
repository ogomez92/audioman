#![windows_subsystem = "windows"]

mod apps_dialog;
mod audio;
mod caster;
mod caster_dialog;
mod devices;
mod dialog;
mod hidden;
mod mediasoup;
mod mirror;
mod mirror_dialog;
mod opus;
mod prism;
mod routing;
mod socketio;
mod wasapi;

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VK_BACK, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, PeekMessageW, SetTimer, MSG, PM_REMOVE, WM_HOTKEY,
    WM_TIMER,
};

const HK_PREV: i32 = 1;
const HK_NEXT: i32 = 2;
const HK_UP: i32 = 3;
const HK_DOWN: i32 = 4;
const HK_QUIT: i32 = 5;
const HK_MIN: i32 = 6;
const HK_MAX: i32 = 7;
const HK_BIG_DOWN: i32 = 8;
const HK_BIG_UP: i32 = 9;
const HK_RESET_ALL: i32 = 10;
const HK_MUTE: i32 = 11;
const HK_DEVICES: i32 = 12;
const HK_TOGGLE_MODE: i32 = 13;
const HK_APPS: i32 = 14;
const HK_MIRROR: i32 = 15;
const HK_CAST: i32 = 16;

const VOLUME_STEP: f32 = 0.02;

// Spoken after the mode name whenever 0 switches modes.
const APP_MODE_HELP: &str = "Left and Right select application, Up and Down change volume, Page Up and Page Down change volume by 20, Home sets 0 percent, End sets 100 percent, M toggles mute, Enter chooses devices for the application, Shift 0 chooses which applications to show, Shift Enter duplicates an output device to another device, Shift S streams a device to a SonicRoom room, Backspace restores all applications to 100 percent, 0 switches to device mode, Q quits.";
const DEVICE_MODE_HELP: &str = "Left and Right select device, Up and Down change device volume, Page Up and Page Down change device volume by 20, Home sets 0 percent, End sets 100 percent, M toggles device mute, Enter enables or disables the device, Shift 0 chooses which devices to show, Shift Enter duplicates the selected output device to another device, Shift S streams the selected device to a SonicRoom room, Backspace restores all applications to 100 percent, 0 switches to application mode, Q quits.";

/// What Left/Right, Up/Down, M and Enter act on.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// Per-application sessions (the default).
    Apps,
    /// Audio endpoints: navigate devices, adjust device volume, Enter
    /// enables/disables.
    Devices,
}

fn mods() -> HOT_KEY_MODIFIERS {
    HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_ALT.0 | MOD_WIN.0 | MOD_NOREPEAT.0)
}

fn shift_mods() -> HOT_KEY_MODIFIERS {
    HOT_KEY_MODIFIERS(mods().0 | MOD_SHIFT.0)
}

fn say(speaker: &Option<prism::Speaker>, text: &str) {
    if let Some(sp) = speaker {
        sp.speak(text);
    }
}

fn announce_current(state: &Arc<Mutex<audio::State>>, speaker: &Option<prism::Speaker>) {
    let guard = state.lock().unwrap();
    let Some(s) = guard.current() else { return };
    let pct = (audio::get_volume(&s.volume) * 100.0).round() as i32;
    let prefix = if audio::get_mute(&s.volume) {
        "muted, "
    } else {
        ""
    };
    say(speaker, &format!("{}{}, {} percent", prefix, s.name, pct));
}

fn navigate(state: &Arc<Mutex<audio::State>>, speaker: &Option<prism::Speaker>, step: i32) {
    let mut guard = state.lock().unwrap();
    let visible = guard.visible_indices();
    if visible.is_empty() {
        drop(guard);
        say(speaker, "No audio applications");
        return;
    }
    let last = visible.len() - 1;
    let pos = guard
        .current_index()
        .and_then(|i| visible.iter().position(|&v| v == i));
    let new_pos = match pos {
        None => 0,
        Some(p) => (p as i32 + step).clamp(0, last as i32) as usize,
    };
    guard.selection = Some(guard.sessions[visible[new_pos]].id);
    drop(guard);
    announce_current(state, speaker);
}

fn volume_suffix(v: f32) -> &'static str {
    if v >= 1.0 {
        " maximum"
    } else if v <= 0.0 {
        " minimum"
    } else {
        ""
    }
}

fn apply_volume<F: Fn(f32) -> f32>(
    state: &Arc<Mutex<audio::State>>,
    speaker: &Option<prism::Speaker>,
    f: F,
) {
    let guard = state.lock().unwrap();
    let Some(s) = guard.current() else {
        drop(guard);
        say(speaker, "Select an application first");
        return;
    };
    let current = audio::get_volume(&s.volume);
    let new_v = f(current).clamp(0.0, 1.0);
    audio::set_volume(&s.volume, new_v);
    drop(guard);
    let pct = (new_v * 100.0).round() as i32;
    say(speaker, &format!("{} percent{}", pct, volume_suffix(new_v)));
}

fn toggle_mute(state: &Arc<Mutex<audio::State>>, speaker: &Option<prism::Speaker>) {
    let guard = state.lock().unwrap();
    let Some(s) = guard.current() else {
        drop(guard);
        say(speaker, "Select an application first");
        return;
    };
    let new_mute = !audio::get_mute(&s.volume);
    audio::set_mute(&s.volume, new_mute);
    let name = s.name.clone();
    drop(guard);
    say(
        speaker,
        &format!("{} {}", if new_mute { "muted" } else { "unmuted" }, name),
    );
}

/// Device-mode navigation state. Selection is kept by endpoint id so it
/// survives re-enumeration. Hidden devices stay in `devices` (so the Shift+0
/// dialog can list them) and are skipped only by `visible_indices()`.
struct DeviceNav {
    devices: Vec<devices::Endpoint>,
    selected: Option<Vec<u16>>,
    hidden: Vec<hidden::HiddenDevice>,
}

impl DeviceNav {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            selected: None,
            hidden: hidden::load_devices(),
        }
    }

    fn is_hidden(&self, e: &devices::Endpoint) -> bool {
        hidden::device_hidden(&self.hidden, &devices::id_string(&e.id))
    }

    /// Indices into `devices` that Left/Right may land on.
    fn visible_indices(&self) -> Vec<usize> {
        (0..self.devices.len())
            .filter(|&i| !self.is_hidden(&self.devices[i]))
            .collect()
    }

    fn refresh(&mut self) {
        self.devices = devices::all_endpoints();
        if let Some(id) = &self.selected {
            if !self.devices.iter().any(|d| &d.id == id) {
                self.selected = None;
            }
        }
    }

    /// Land on the current default output (or the first device) when nothing
    /// is selected yet.
    fn select_initial(&mut self) {
        if self.selected.is_some() {
            return;
        }
        let default = devices::default_output_id();
        let pick = default
            .and_then(|id| self.devices.iter().find(|d| d.id == id))
            .filter(|d| !self.is_hidden(d))
            .or_else(|| self.devices.iter().find(|d| !self.is_hidden(d)));
        self.selected = pick.map(|d| d.id.clone());
    }

    fn index(&self) -> Option<usize> {
        self.selected
            .as_ref()
            .and_then(|id| self.devices.iter().position(|d| &d.id == id))
    }

    fn current(&self) -> Option<&devices::Endpoint> {
        self.index().map(|i| &self.devices[i])
    }
}

fn device_description(e: &devices::Endpoint) -> String {
    let kind = if e.is_input { "input" } else { "output" };
    let mut desc = format!("{}, {}", e.name, kind);
    if !e.enabled {
        desc.push_str(", disabled");
        return desc;
    }
    desc.push_str(", enabled");
    if devices::endpoint_mute(&e.id) == Some(true) {
        desc.push_str(", muted");
    }
    if let Some(v) = devices::endpoint_volume(&e.id) {
        desc.push_str(&format!(", {} percent", (v * 100.0).round() as i32));
    }
    desc
}

fn navigate_devices(nav: &mut DeviceNav, speaker: &Option<prism::Speaker>, step: i32) {
    nav.refresh();
    let visible = nav.visible_indices();
    if visible.is_empty() {
        say(speaker, "No audio devices");
        return;
    }
    let last = visible.len() - 1;
    let pos = nav
        .index()
        .and_then(|i| visible.iter().position(|&v| v == i));
    let new_pos = match pos {
        None => 0,
        Some(p) => (p as i32 + step).clamp(0, last as i32) as usize,
    };
    let new_idx = visible[new_pos];
    nav.selected = Some(nav.devices[new_idx].id.clone());
    say(speaker, &device_description(&nav.devices[new_idx]));
}

fn apply_device_volume<F: Fn(f32) -> f32>(
    nav: &DeviceNav,
    speaker: &Option<prism::Speaker>,
    f: F,
) {
    let Some(e) = nav.current() else {
        say(speaker, "Select a device first");
        return;
    };
    let Some(current) = devices::endpoint_volume(&e.id) else {
        say(speaker, "Device volume unavailable");
        return;
    };
    let new_v = f(current).clamp(0.0, 1.0);
    if !devices::set_endpoint_volume(&e.id, new_v) {
        say(speaker, "Device volume unavailable");
        return;
    }
    let pct = (new_v * 100.0).round() as i32;
    say(speaker, &format!("{} percent{}", pct, volume_suffix(new_v)));
}

fn toggle_device_mute(nav: &DeviceNav, speaker: &Option<prism::Speaker>) {
    let Some(e) = nav.current() else {
        say(speaker, "Select a device first");
        return;
    };
    let Some(muted) = devices::endpoint_mute(&e.id) else {
        say(speaker, "Device unavailable");
        return;
    };
    if devices::set_endpoint_mute(&e.id, !muted) {
        say(
            speaker,
            &format!("{} {}", if !muted { "muted" } else { "unmuted" }, e.name),
        );
    } else {
        say(speaker, "Device unavailable");
    }
}

fn toggle_device_enabled(nav: &mut DeviceNav, speaker: &Option<prism::Speaker>) {
    let Some(idx) = nav.index() else {
        say(speaker, "Select a device first");
        return;
    };
    let (id, name, enable) = {
        let e = &nav.devices[idx];
        (e.id.clone(), e.name.clone(), !e.enabled)
    };
    if devices::set_endpoint_enabled(&id, enable) {
        nav.devices[idx].enabled = enable;
        say(
            speaker,
            &format!("{} {}", if enable { "Enabled" } else { "Disabled" }, name),
        );
    } else {
        say(
            speaker,
            &format!(
                "Could not {} {}",
                if enable { "enable" } else { "disable" },
                name
            ),
        );
    }
}

/// Open the picker for the currently selected application and apply per-app
/// routing (system-wide defaults for the pid-0 system-sounds session, which
/// has no owning process to route).
fn open_device_dialog(state: &Arc<Mutex<audio::State>>, speaker: &Option<prism::Speaker>) {
    let (pid, app_name) = {
        let guard = state.lock().unwrap();
        match guard.current() {
            Some(s) => (s.pid, s.name.clone()),
            None => {
                drop(guard);
                say(speaker, "Select an application first");
                return;
            }
        }
    };

    let inputs = devices::inputs();
    let outputs = devices::outputs();

    // Open the combos on the app's current routing (0 = the "Default" entry).
    let preselect = |list: &[devices::DeviceInfo], routed: Option<String>| -> usize {
        routed
            .and_then(|rid| {
                list.iter()
                    .position(|d| devices::id_string(&d.id).eq_ignore_ascii_case(&rid))
            })
            .map(|i| i + 1)
            .unwrap_or(0)
    };
    let pre_in = preselect(&inputs, routing::app_device_id(pid, true));
    let pre_out = preselect(&outputs, routing::app_device_id(pid, false));

    let Some(sel) = dialog::show(&app_name, &inputs, pre_in, &outputs, pre_out) else {
        return; // cancelled
    };

    if pid == 0 {
        let mut parts: Vec<String> = Vec::new();
        if let dialog::Choice::Device { id, name } = &sel.output {
            if devices::set_default(id) {
                parts.push(format!("default output {}", name));
            }
        }
        if let dialog::Choice::Device { id, name } = &sel.input {
            if devices::set_default(id) {
                parts.push(format!("default input {}", name));
            }
        }
        if parts.is_empty() {
            say(speaker, "No devices changed");
        } else {
            say(speaker, &format!("Set {}", parts.join(", ")));
        }
        return;
    }

    let apply = |input: bool, choice: &dialog::Choice| -> String {
        match choice {
            dialog::Choice::Default => {
                if routing::set_app_device(pid, input, None) {
                    "default".to_string()
                } else {
                    "failed".to_string()
                }
            }
            dialog::Choice::Device { id, name } => {
                if routing::set_app_device(pid, input, Some(id)) {
                    name.clone()
                } else {
                    "failed".to_string()
                }
            }
        }
    };
    let out_desc = apply(false, &sel.output);
    let in_desc = apply(true, &sel.input);
    say(
        speaker,
        &format!("{}: output {}, input {}", app_name, out_desc, in_desc),
    );
}

/// Open the visibility dialog: running apps plus any remembered hidden ones,
/// checked = shown. Accepting persists the unchecked names.
fn open_apps_dialog(state: &Arc<Mutex<audio::State>>, speaker: &Option<prism::Speaker>) {
    let mut entries: Vec<apps_dialog::AppEntry> = Vec::new();
    {
        let guard = state.lock().unwrap();
        for s in &guard.sessions {
            if !entries.iter().any(|e| hidden::same_name(&e.name, &s.name)) {
                entries.push(apps_dialog::AppEntry {
                    name: s.name.clone(),
                    shown: !guard.is_hidden(s),
                });
            }
        }
        // Hidden apps that aren't running stay listed so they can be unhidden.
        for name in &guard.hidden {
            if !entries.iter().any(|e| hidden::same_name(&e.name, name)) {
                entries.push(apps_dialog::AppEntry {
                    name: name.clone(),
                    shown: false,
                });
            }
        }
    }
    if entries.is_empty() {
        say(speaker, "No audio applications");
        return;
    }
    entries.sort_by_key(|e| e.name.to_lowercase());

    let Some(shown) = apps_dialog::show(&entries) else {
        return; // cancelled
    };
    let hidden_names: Vec<String> = entries
        .iter()
        .zip(&shown)
        .filter(|(_, &shown)| !shown)
        .map(|(e, _)| e.name.clone())
        .collect();
    let saved = hidden::save(&hidden_names);
    let count = hidden_names.len();
    {
        let mut guard = state.lock().unwrap();
        guard.hidden = hidden_names;
        let selection_hidden = guard.current().is_some_and(|s| guard.is_hidden(s));
        if selection_hidden {
            guard.selection = None;
        }
    }
    if saved.is_err() {
        say(speaker, "Could not save hidden applications");
    } else if count == 0 {
        say(speaker, "All applications shown");
    } else if count == 1 {
        say(speaker, "1 application hidden");
    } else {
        say(speaker, &format!("{} applications hidden", count));
    }
}

/// Device-mode Shift+0: the same checklist, listing present devices plus any
/// remembered hidden ones that are unplugged. Accepting persists the unchecked
/// devices by endpoint id.
fn open_devices_dialog(nav: &mut DeviceNav, speaker: &Option<prism::Speaker>) {
    nav.refresh();
    let mut entries: Vec<apps_dialog::AppEntry> = Vec::new();
    let mut ids: Vec<String> = Vec::new();
    for e in &nav.devices {
        let kind = if e.is_input { "input" } else { "output" };
        entries.push(apps_dialog::AppEntry {
            name: format!("{} ({})", e.name, kind),
            shown: !nav.is_hidden(e),
        });
        ids.push(devices::id_string(&e.id));
    }
    for h in &nav.hidden {
        if !ids.iter().any(|id| id.eq_ignore_ascii_case(&h.id)) {
            let name = if h.name.is_empty() {
                h.id.clone()
            } else {
                h.name.clone()
            };
            entries.push(apps_dialog::AppEntry {
                name: format!("{} (not connected)", name),
                shown: false,
            });
            ids.push(h.id.clone());
        }
    }
    if entries.is_empty() {
        say(speaker, "No audio devices");
        return;
    }

    let Some(shown) = apps_dialog::show_devices(&entries) else {
        return; // cancelled
    };
    let hidden_devices: Vec<hidden::HiddenDevice> = ids
        .iter()
        .zip(&shown)
        .filter(|(_, &shown)| !shown)
        .map(|(id, _)| {
            let name = nav
                .devices
                .iter()
                .find(|e| devices::id_string(&e.id).eq_ignore_ascii_case(id))
                .map(|e| e.name.clone())
                .or_else(|| {
                    nav.hidden
                        .iter()
                        .find(|h| h.id.eq_ignore_ascii_case(id))
                        .map(|h| h.name.clone())
                })
                .unwrap_or_default();
            hidden::HiddenDevice {
                id: id.clone(),
                name,
            }
        })
        .collect();
    let saved = hidden::save_devices(&hidden_devices);
    let count = hidden_devices.len();
    nav.hidden = hidden_devices;
    if nav.current().is_some_and(|e| nav.is_hidden(e)) {
        nav.selected = None;
    }
    if saved.is_err() {
        say(speaker, "Could not save hidden devices");
    } else if count == 0 {
        say(speaker, "All devices shown");
    } else if count == 1 {
        say(speaker, "1 device hidden");
    } else {
        say(speaker, &format!("{} devices hidden", count));
    }
}

/// Milliseconds between attempts to resume a wanted duplication or room
/// stream that isn't running (a device is unplugged, the engine failed, the
/// server is unreachable).
const MIRROR_RETRY_MS: u32 = 5000;

/// Output duplication: what the user asked for (`wanted`, persisted in
/// `active_mirror.txt`) and the engine currently carrying it out. A wanted
/// duplication that isn't running is retried on a timer and on every device
/// change, so unplugging a device pauses it and plugging it back resumes it.
struct Duplication {
    wanted: Option<mirror::Active>,
    running: Option<mirror::Mirror>,
    /// Consecutive automatic starts that failed with both devices present.
    failures: u32,
    /// Last start error spoken, so a device that keeps refusing is announced
    /// once rather than every retry.
    spoken_error: Option<String>,
}

impl Duplication {
    /// Startup: pick up the duplication that was wanted when audioman last
    /// exited and start it if both devices are there.
    fn load(speaker: &Option<prism::Speaker>, main_thread: u32) -> Duplication {
        let mut dup = Duplication {
            wanted: mirror::Active::load(),
            running: None,
            failures: 0,
            spoken_error: None,
        };
        dup.try_resume(speaker, main_thread);
        if dup.running.is_none() {
            if let Some(w) = &dup.wanted {
                say(
                    speaker,
                    &format!(
                        "Duplicating {} to {} will start when both devices are available",
                        w.source_name, w.target_name
                    ),
                );
            }
        }
        dup
    }

    /// Start the wanted duplication if it isn't running and both of its
    /// devices are active outputs. A missing device is not an error: it just
    /// waits for the next retry.
    fn try_resume(&mut self, speaker: &Option<prism::Speaker>, main_thread: u32) {
        // WM_MIRROR_STOPPED is lost if it arrives while a dialog's nested
        // loop runs; catch a dead engine here instead.
        self.on_stopped(speaker);
        if self.running.is_some() {
            return;
        }
        let Some(wanted) = self.wanted.as_mut() else {
            return;
        };
        let outputs = devices::outputs();
        let find = |id: &str| {
            outputs
                .iter()
                .find(|d| devices::id_string(&d.id).eq_ignore_ascii_case(id))
        };
        let (Some(source), Some(target)) = (find(&wanted.source_id), find(&wanted.target_id))
        else {
            // Only count failures that happen with both devices present.
            self.failures = 0;
            return;
        };
        match mirror::Mirror::start(
            &source.id,
            &source.name,
            &target.id,
            &target.name,
            wanted.latency,
            main_thread,
        ) {
            Ok(m) => {
                wanted.source_name = source.name.clone();
                wanted.target_name = target.name.clone();
                wanted.save();
                say(
                    speaker,
                    &format!(
                        "Resumed duplicating {} to {}, about {} milliseconds delay",
                        source.name,
                        target.name,
                        m.stats.estimated_delay_ms()
                    ),
                );
                self.running = Some(m);
                self.failures = 0;
                self.spoken_error = None;
            }
            Err(e) => {
                // A just-plugged-in device often refuses its first start
                // while it settles; speak only when the failure persists.
                self.failures += 1;
                if self.failures >= 2 && self.spoken_error.as_deref() != Some(e.as_str()) {
                    say(
                        speaker,
                        &format!(
                            "Could not resume duplicating {} to {}: {}. Still retrying.",
                            source.name, target.name, e
                        ),
                    );
                    self.spoken_error = Some(e);
                }
            }
        }
    }

    /// If the engine stopped on its own, drop it but keep wanting it; retries
    /// resume it once the devices are usable again. Checks the engine's own
    /// failure flag, so a late WM_MIRROR_STOPPED from an engine already
    /// replaced can't stop the healthy one.
    fn on_stopped(&mut self, speaker: &Option<prism::Speaker>) {
        let Some(reason) = self.running.as_ref().and_then(|m| m.failure()) else {
            return;
        };
        let Some(m) = self.running.take() else {
            return;
        };
        let target = m.target_name.clone();
        drop(m);
        self.failures = 0;
        self.spoken_error = None;
        say(
            speaker,
            &format!(
                "Duplication to {} stopped: {}. It will resume when both devices are available.",
                target, reason
            ),
        );
    }

    /// Shift+Enter: stop the running or waiting duplication, or ask for a
    /// new one. Returns whether the dialog was shown.
    fn toggle(
        &mut self,
        nav: &DeviceNav,
        mode: Mode,
        speaker: &Option<prism::Speaker>,
        main_thread: u32,
    ) -> bool {
        self.failures = 0;
        self.spoken_error = None;
        if let Some(m) = self.running.take() {
            let desc = format!("{} to {}", m.source_name, m.target_name);
            drop(m);
            self.wanted = None;
            mirror::Active::clear();
            say(speaker, &format!("Stopped duplicating {}", desc));
            false
        } else if let Some(w) = self.wanted.take() {
            mirror::Active::clear();
            say(
                speaker,
                &format!(
                    "Cancelled duplicating {} to {}, which was waiting for its devices",
                    w.source_name, w.target_name
                ),
            );
            false
        } else {
            if let Some((active, m)) = open_mirror_dialog(nav, mode, speaker, main_thread) {
                active.save();
                self.wanted = Some(active);
                self.running = Some(m);
            }
            true
        }
    }
}

/// Shift+Enter with no duplication wanted: ask for source / target / latency
/// and start the mirror engine. The source combo opens on the device selected
/// in device mode (or the default output), the target on the last-used one.
fn open_mirror_dialog(
    nav: &DeviceNav,
    mode: Mode,
    speaker: &Option<prism::Speaker>,
    main_thread: u32,
) -> Option<(mirror::Active, mirror::Mirror)> {
    let outputs = devices::outputs();
    if outputs.len() < 2 {
        say(speaker, "Duplicating needs at least two output devices");
        return None;
    }

    let find = |id: &[u16]| -> Option<usize> {
        let wanted = devices::id_string(id);
        outputs
            .iter()
            .position(|d| devices::id_string(&d.id).eq_ignore_ascii_case(&wanted))
    };
    let source_idx = match mode {
        Mode::Devices => nav
            .current()
            .filter(|e| !e.is_input && e.enabled)
            .and_then(|e| find(&e.id)),
        Mode::Apps => None,
    }
    .or_else(|| devices::default_output_id().and_then(|id| find(&id)))
    .unwrap_or(0);

    let prefs = mirror::Prefs::load();
    let target_idx = prefs
        .target_id
        .as_deref()
        .and_then(|saved| {
            outputs
                .iter()
                .position(|d| devices::id_string(&d.id).eq_ignore_ascii_case(saved))
        })
        .filter(|&i| i != source_idx)
        .or_else(|| (0..outputs.len()).find(|&i| i != source_idx))
        .unwrap_or(0);

    let choice = mirror_dialog::show(&outputs, source_idx, target_idx, prefs.latency)?;
    if choice.source == choice.target {
        say(speaker, "Choose two different devices");
        return None;
    }
    let source = &outputs[choice.source];
    let target = &outputs[choice.target];
    mirror::Prefs::save(&target.id, choice.latency);
    match mirror::Mirror::start(
        &source.id,
        &source.name,
        &target.id,
        &target.name,
        choice.latency,
        main_thread,
    ) {
        Ok(m) => {
            say(
                speaker,
                &format!(
                    "Duplicating {} to {}, {}, about {} milliseconds delay. Press Shift Enter again to stop.",
                    source.name,
                    target.name,
                    choice.latency.name(),
                    m.stats.estimated_delay_ms()
                ),
            );
            let active = mirror::Active {
                source_id: devices::id_string(&source.id),
                target_id: devices::id_string(&target.id),
                latency: choice.latency,
                source_name: source.name.clone(),
                target_name: target.name.clone(),
            };
            Some((active, m))
        }
        Err(e) => {
            say(speaker, &format!("Could not start duplication: {}", e));
            None
        }
    }
}

/// Streaming a device into a SonicRoom room: what the user asked for
/// (`wanted`, persisted in `active_sonicroom.txt` once it first goes live)
/// and the engine carrying it out. Like [`Duplication`], a wanted stream that
/// isn't running is retried on the timer and on every device change, so a
/// dropped connection or an unplugged device pauses it rather than ending it.
struct Casting {
    wanted: Option<caster::Settings>,
    running: Option<caster::Caster>,
    /// Whether `wanted` has ever gone live. Until it has, a failure means the
    /// start itself failed (wrong server, room refused, ...) and drops it
    /// instead of retrying forever.
    proven: bool,
    /// Whether the running engine going live has been announced.
    announced: bool,
    /// Consecutive automatic attempts that failed before going live.
    failures: u32,
    /// Last retry error spoken, so a server that stays down is announced once.
    spoken_error: Option<String>,
}

impl Casting {
    /// Startup: pick up the stream that was wanted when audioman last exited.
    fn load(speaker: &Option<prism::Speaker>, main_thread: u32) -> Casting {
        let mut casting = Casting {
            wanted: caster::Settings::load_active(),
            running: None,
            proven: true,
            announced: false,
            failures: 0,
            spoken_error: None,
        };
        casting.try_resume(speaker, main_thread);
        if casting.running.is_none() {
            if let Some(w) = &casting.wanted {
                say(
                    speaker,
                    &format!(
                        "Streaming {} to room {} will start when the device is available",
                        w.device_name, w.room
                    ),
                );
            }
        }
        casting
    }

    /// Start the wanted stream if it isn't running and its device is active.
    /// A missing device is not an error: it just waits for the next retry.
    fn try_resume(&mut self, speaker: &Option<prism::Speaker>, main_thread: u32) {
        // WM_CASTER_CHANGED is lost if it arrives while a dialog's nested
        // loop runs; catch up here.
        self.on_changed(speaker);
        if self.running.is_some() || !self.proven {
            return;
        }
        let Some(wanted) = &self.wanted else {
            return;
        };
        let sources = caster::sources();
        let Some(source) = sources
            .iter()
            .find(|s| devices::id_string(&s.id).eq_ignore_ascii_case(&wanted.device_id))
        else {
            // Only count failures that happen with the device present.
            self.failures = 0;
            return;
        };
        match caster::Caster::start(wanted, source, main_thread) {
            Ok(c) => {
                self.running = Some(c);
                self.announced = false;
            }
            Err(e) => self.retry_failed(speaker, e),
        }
    }

    /// An automatic attempt failed before going live. A just-plugged-in
    /// device or a restarting server often refuses once; speak only when the
    /// failure persists, and each distinct error once.
    fn retry_failed(&mut self, speaker: &Option<prism::Speaker>, error: String) {
        self.failures += 1;
        if self.failures >= 2 && self.spoken_error.as_deref() != Some(error.as_str()) {
            let room = self.wanted.as_ref().map(|w| w.room.as_str()).unwrap_or("");
            say(
                speaker,
                &format!(
                    "Could not resume streaming to room {}: {}. Still retrying.",
                    room, error
                ),
            );
            self.spoken_error = Some(error);
        }
    }

    /// Follow the engine: announce it going live, drop it when it stops.
    /// Reads the engine's own phase, so a late message from an engine that
    /// was already replaced does nothing.
    fn on_changed(&mut self, speaker: &Option<prism::Speaker>) {
        let Some(phase) = self.running.as_ref().map(|c| c.phase()) else {
            return;
        };
        match phase {
            caster::Phase::Connecting => {}
            caster::Phase::Live => {
                let Some(c) = self.running.as_ref().filter(|_| !self.announced) else {
                    return;
                };
                self.announced = true;
                self.failures = 0;
                self.spoken_error = None;
                let s = &c.settings;
                // Persisted from the first live moment on, refreshing the
                // device name in case it changed.
                s.save_active();
                self.wanted = Some(s.clone());
                if self.proven {
                    say(
                        speaker,
                        &format!("Resumed streaming {} to room {}", s.device_name, s.room),
                    );
                } else {
                    self.proven = true;
                    say(
                        speaker,
                        &format!(
                            "Streaming {} to room {}, {}, about {} milliseconds before the network. Press Shift S again to stop.",
                            s.device_name,
                            s.room,
                            s.quality.name(),
                            c.stats.estimated_delay_ms(s.quality)
                        ),
                    );
                }
            }
            caster::Phase::Failed(reason) => {
                let room = self
                    .running
                    .take()
                    .map(|c| c.settings.room.clone())
                    .unwrap_or_default();
                if !self.proven {
                    self.wanted = None;
                    say(
                        speaker,
                        &format!("Could not stream to room {}: {}", room, reason),
                    );
                } else if self.announced {
                    self.failures = 0;
                    self.spoken_error = None;
                    say(
                        speaker,
                        &format!(
                            "Streaming to room {} stopped: {}. It will resume when possible.",
                            room, reason
                        ),
                    );
                } else {
                    self.retry_failed(speaker, reason);
                }
            }
            caster::Phase::Removed => {
                let room = self
                    .running
                    .take()
                    .map(|c| c.settings.room.clone())
                    .unwrap_or_default();
                self.wanted = None;
                caster::Settings::clear_active();
                say(
                    speaker,
                    &format!(
                        "Someone in room {} removed the stream. Streaming stopped.",
                        room
                    ),
                );
            }
        }
    }

    /// Shift+S: stop the running or waiting stream, or ask for a new one.
    /// Returns whether the dialog was shown.
    fn toggle(
        &mut self,
        nav: &DeviceNav,
        mode: Mode,
        speaker: &Option<prism::Speaker>,
        main_thread: u32,
    ) -> bool {
        self.failures = 0;
        self.spoken_error = None;
        if let Some(c) = self.running.take() {
            let desc = format!("{} to room {}", c.settings.device_name, c.settings.room);
            drop(c);
            self.wanted = None;
            caster::Settings::clear_active();
            say(speaker, &format!("Stopped streaming {}", desc));
            false
        } else if let Some(w) = self.wanted.take() {
            caster::Settings::clear_active();
            say(
                speaker,
                &format!(
                    "Cancelled streaming {} to room {}, which was waiting for its device",
                    w.device_name, w.room
                ),
            );
            false
        } else {
            if let Some(c) = open_cast_dialog(nav, mode, speaker, main_thread) {
                self.wanted = Some(c.settings.clone());
                self.running = Some(c);
                self.proven = false;
                self.announced = false;
            }
            true
        }
    }
}

/// Shift+S with no stream wanted: ask where and what to stream, remember the
/// answers, and start connecting. The device combo opens on the device
/// selected in device mode, else the last one streamed, else the default
/// output.
fn open_cast_dialog(
    nav: &DeviceNav,
    mode: Mode,
    speaker: &Option<prism::Speaker>,
    main_thread: u32,
) -> Option<caster::Caster> {
    let sources = caster::sources();
    if sources.is_empty() {
        say(speaker, "No audio devices to stream");
        return None;
    }
    let mut prefs = caster::Settings::load_prefs();
    let find = |id: &str| {
        sources
            .iter()
            .position(|s| devices::id_string(&s.id).eq_ignore_ascii_case(id))
    };
    let device_idx = match mode {
        Mode::Devices => nav
            .current()
            .filter(|e| e.enabled)
            .and_then(|e| find(&devices::id_string(&e.id))),
        Mode::Apps => None,
    }
    .or_else(|| find(&prefs.device_id))
    .or_else(|| devices::default_output_id().and_then(|id| find(&devices::id_string(&id))))
    .unwrap_or(0);

    let choice = caster_dialog::show(&sources, device_idx, &prefs)?;
    let source = &sources[choice.device];
    // A pasted room link names the room too.
    prefs.room = if choice.room.is_empty() {
        socketio::room_from_link(&choice.server).unwrap_or_default()
    } else {
        choice.room
    };
    prefs.server = choice.server;
    if !choice.name.is_empty() {
        prefs.name = choice.name;
    }
    prefs.device_id = devices::id_string(&source.id);
    prefs.device_name = source.name.clone();
    prefs.channels = choice.channels;
    prefs.quality = choice.quality;
    // Remember the answers even when they are rejected below, so fixing a
    // typo doesn't mean retyping everything.
    prefs.save_prefs();

    if prefs.server.is_empty() {
        say(speaker, "Enter the SonicRoom server address or a room link");
        return None;
    }
    if !caster::valid_room(&prefs.room) {
        say(
            speaker,
            "Enter a room name using only letters, digits, hyphens and underscores",
        );
        return None;
    }
    match caster::Caster::start(&prefs, source, main_thread) {
        Ok(c) => {
            say(speaker, &format!("Connecting to room {}", prefs.room));
            Some(c)
        }
        Err(e) => {
            say(speaker, &format!("Could not start streaming: {}", e));
            None
        }
    }
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let main_thread = unsafe { GetCurrentThreadId() };

    let speaker = prism::Speaker::new();
    let mut manager = match audio::AudioManager::new(main_thread) {
        Ok(m) => m,
        Err(e) => {
            say(&speaker, &format!("Audio manager failed to start: {}", e));
            return;
        }
    };
    let state = manager.state.clone();
    let mut mode = Mode::Apps;
    let mut nav = DeviceNav::new();

    unsafe {
        let h = HWND(std::ptr::null_mut());
        let _ = RegisterHotKey(h, HK_PREV, mods(), VK_LEFT.0 as u32);
        let _ = RegisterHotKey(h, HK_NEXT, mods(), VK_RIGHT.0 as u32);
        let _ = RegisterHotKey(h, HK_UP, mods(), VK_UP.0 as u32);
        let _ = RegisterHotKey(h, HK_DOWN, mods(), VK_DOWN.0 as u32);
        let _ = RegisterHotKey(h, HK_QUIT, mods(), b'Q' as u32);
        let _ = RegisterHotKey(h, HK_MIN, mods(), VK_HOME.0 as u32);
        let _ = RegisterHotKey(h, HK_MAX, mods(), VK_END.0 as u32);
        let _ = RegisterHotKey(h, HK_BIG_UP, mods(), VK_PRIOR.0 as u32);
        let _ = RegisterHotKey(h, HK_BIG_DOWN, mods(), VK_NEXT.0 as u32);
        let _ = RegisterHotKey(h, HK_RESET_ALL, mods(), VK_BACK.0 as u32);
        let _ = RegisterHotKey(h, HK_MUTE, mods(), b'M' as u32);
        let _ = RegisterHotKey(h, HK_DEVICES, mods(), VK_RETURN.0 as u32);
        let _ = RegisterHotKey(h, HK_TOGGLE_MODE, mods(), b'0' as u32);
        let _ = RegisterHotKey(h, HK_APPS, shift_mods(), b'0' as u32);
        let _ = RegisterHotKey(h, HK_MIRROR, shift_mods(), VK_RETURN.0 as u32);
        let _ = RegisterHotKey(h, HK_CAST, shift_mods(), b'S' as u32);
    }

    say(&speaker, "Audio manager ready");
    let mut duplication = Duplication::load(&speaker, main_thread);
    let mut casting = Casting::load(&speaker, main_thread);
    // Thread timer (null hwnd): WM_TIMER lands in the thread queue carrying
    // the id SetTimer returns.
    let retry_timer = unsafe { SetTimer(HWND(std::ptr::null_mut()), 0, MIRROR_RETRY_MS, None) };

    let mut msg = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut msg, HWND(std::ptr::null_mut()), 0, 0) };
        if !got.as_bool() {
            break;
        }

        if msg.message == WM_HOTKEY {
            match msg.wParam.0 as i32 {
                HK_QUIT => {
                    say(&speaker, "Goodbye");
                    break;
                }
                HK_PREV => match mode {
                    Mode::Apps => navigate(&state, &speaker, -1),
                    Mode::Devices => navigate_devices(&mut nav, &speaker, -1),
                },
                HK_NEXT => match mode {
                    Mode::Apps => navigate(&state, &speaker, 1),
                    Mode::Devices => navigate_devices(&mut nav, &speaker, 1),
                },
                HK_UP => match mode {
                    Mode::Apps => apply_volume(&state, &speaker, |v| v + VOLUME_STEP),
                    Mode::Devices => apply_device_volume(&nav, &speaker, |v| v + VOLUME_STEP),
                },
                HK_DOWN => match mode {
                    Mode::Apps => apply_volume(&state, &speaker, |v| v - VOLUME_STEP),
                    Mode::Devices => apply_device_volume(&nav, &speaker, |v| v - VOLUME_STEP),
                },
                HK_BIG_UP => match mode {
                    Mode::Apps => apply_volume(&state, &speaker, |v| v + 0.20),
                    Mode::Devices => apply_device_volume(&nav, &speaker, |v| v + 0.20),
                },
                HK_BIG_DOWN => match mode {
                    Mode::Apps => apply_volume(&state, &speaker, |v| v - 0.20),
                    Mode::Devices => apply_device_volume(&nav, &speaker, |v| v - 0.20),
                },
                HK_MIN => match mode {
                    Mode::Apps => apply_volume(&state, &speaker, |_| 0.0),
                    Mode::Devices => apply_device_volume(&nav, &speaker, |_| 0.0),
                },
                HK_MAX => match mode {
                    Mode::Apps => apply_volume(&state, &speaker, |_| 1.0),
                    Mode::Devices => apply_device_volume(&nav, &speaker, |_| 1.0),
                },
                HK_MUTE => match mode {
                    Mode::Apps => toggle_mute(&state, &speaker),
                    Mode::Devices => toggle_device_mute(&nav, &speaker),
                },
                HK_DEVICES => match mode {
                    Mode::Apps => {
                        open_device_dialog(&state, &speaker);
                        // Device notifications that arrived while the nested
                        // dialog loop ran were dropped; rebuild to catch up.
                        manager.rebuild();
                    }
                    Mode::Devices => toggle_device_enabled(&mut nav, &speaker),
                },
                HK_TOGGLE_MODE => {
                    mode = match mode {
                        Mode::Apps => {
                            nav.refresh();
                            nav.select_initial();
                            match nav.current() {
                                Some(e) => say(
                                    &speaker,
                                    &format!(
                                        "Device mode, {}. {}",
                                        device_description(e),
                                        DEVICE_MODE_HELP
                                    ),
                                ),
                                None => say(
                                    &speaker,
                                    &format!("Device mode, no audio devices. {}", DEVICE_MODE_HELP),
                                ),
                            }
                            Mode::Devices
                        }
                        Mode::Devices => {
                            let name = state.lock().unwrap().current().map(|s| s.name.clone());
                            match name {
                                Some(n) => say(
                                    &speaker,
                                    &format!("Application mode, {}. {}", n, APP_MODE_HELP),
                                ),
                                None => say(
                                    &speaker,
                                    &format!("Application mode. {}", APP_MODE_HELP),
                                ),
                            }
                            Mode::Apps
                        }
                    };
                }
                HK_APPS => {
                    match mode {
                        Mode::Apps => open_apps_dialog(&state, &speaker),
                        Mode::Devices => open_devices_dialog(&mut nav, &speaker),
                    }
                    // Same as the device picker: catch up on device
                    // notifications dropped by the nested dialog loop.
                    manager.rebuild();
                }
                HK_MIRROR => {
                    if duplication.toggle(&nav, mode, &speaker, main_thread) {
                        // Catch up on device notifications dropped by the
                        // nested dialog loop.
                        manager.rebuild();
                    }
                }
                HK_CAST => {
                    if casting.toggle(&nav, mode, &speaker, main_thread) {
                        // Catch up on device notifications dropped by the
                        // nested dialog loop.
                        manager.rebuild();
                    }
                }
                HK_RESET_ALL => {
                    let n = audio::restore_all_max(&state);
                    say(
                        &speaker,
                        &format!("Restored {} applications to 100 percent", n),
                    );
                }
                _ => {}
            }
        } else if msg.message == audio::WM_DEVICES_CHANGED && msg.hwnd.0.is_null() {
            // Device changes arrive in bursts (one notification per role /
            // state transition); drain the queue and rebuild once.
            let mut pending = MSG::default();
            while unsafe {
                PeekMessageW(
                    &mut pending,
                    HWND(std::ptr::null_mut()),
                    audio::WM_DEVICES_CHANGED,
                    audio::WM_DEVICES_CHANGED,
                    PM_REMOVE,
                )
            }
            .as_bool()
            {}
            manager.rebuild();
            if mode == Mode::Devices {
                nav.refresh();
            }
            // A device coming back resumes a paused duplication or stream
            // right away instead of on the next retry tick.
            duplication.try_resume(&speaker, main_thread);
            casting.try_resume(&speaker, main_thread);
        } else if msg.message == mirror::WM_MIRROR_STOPPED && msg.hwnd.0.is_null() {
            duplication.on_stopped(&speaker);
        } else if msg.message == caster::WM_CASTER_CHANGED && msg.hwnd.0.is_null() {
            casting.on_changed(&speaker);
        } else if msg.message == WM_TIMER
            && msg.hwnd.0.is_null()
            && retry_timer != 0
            && msg.wParam.0 == retry_timer
        {
            duplication.try_resume(&speaker, main_thread);
            casting.try_resume(&speaker, main_thread);
        }

        unsafe {
            DispatchMessageW(&msg);
        }
    }

    unsafe {
        let h = HWND(std::ptr::null_mut());
        let _ = UnregisterHotKey(h, HK_PREV);
        let _ = UnregisterHotKey(h, HK_NEXT);
        let _ = UnregisterHotKey(h, HK_UP);
        let _ = UnregisterHotKey(h, HK_DOWN);
        let _ = UnregisterHotKey(h, HK_QUIT);
        let _ = UnregisterHotKey(h, HK_MIN);
        let _ = UnregisterHotKey(h, HK_MAX);
        let _ = UnregisterHotKey(h, HK_BIG_DOWN);
        let _ = UnregisterHotKey(h, HK_BIG_UP);
        let _ = UnregisterHotKey(h, HK_RESET_ALL);
        let _ = UnregisterHotKey(h, HK_MUTE);
        let _ = UnregisterHotKey(h, HK_DEVICES);
        let _ = UnregisterHotKey(h, HK_TOGGLE_MODE);
        let _ = UnregisterHotKey(h, HK_APPS);
        let _ = UnregisterHotKey(h, HK_MIRROR);
        let _ = UnregisterHotKey(h, HK_CAST);
        if retry_timer != 0 {
            let _ = KillTimer(h, retry_timer);
        }
    }

    drop(casting);
    drop(duplication);
    drop(manager);
    drop(speaker);

    unsafe {
        CoUninitialize();
    }
}
