#![windows_subsystem = "windows"]

mod apps_dialog;
mod audio;
mod devices;
mod dialog;
mod hidden;
mod mirror;
mod mirror_dialog;
mod prism;
mod routing;

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
    DispatchMessageW, GetMessageW, PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY,
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

const VOLUME_STEP: f32 = 0.02;

// Spoken after the mode name whenever 0 switches modes.
const APP_MODE_HELP: &str = "Left and Right select application, Up and Down change volume, Page Up and Page Down change volume by 20, Home sets 0 percent, End sets 100 percent, M toggles mute, Enter chooses devices for the application, Shift 0 chooses which applications to show, Shift Enter duplicates an output device to another device, Backspace restores all applications to 100 percent, 0 switches to device mode, Q quits.";
const DEVICE_MODE_HELP: &str = "Left and Right select device, Up and Down change device volume, Page Up and Page Down change device volume by 20, Home sets 0 percent, End sets 100 percent, M toggles device mute, Enter enables or disables the device, Shift 0 chooses which devices to show, Shift Enter duplicates the selected output device to another device, Backspace restores all applications to 100 percent, 0 switches to application mode, Q quits.";

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

/// Shift+Enter with no duplication running: ask for source / target / latency
/// and start the mirror engine. The source combo opens on the device selected
/// in device mode (or the default output), the target on the last-used one.
fn open_mirror_dialog(
    nav: &DeviceNav,
    mode: Mode,
    speaker: &Option<prism::Speaker>,
    main_thread: u32,
) -> Option<mirror::Mirror> {
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
            Some(m)
        }
        Err(e) => {
            say(speaker, &format!("Could not start duplication: {}", e));
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
    let mut mirror: Option<mirror::Mirror> = None;

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
    }

    say(&speaker, "Audio manager ready");

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
                    if let Some(m) = mirror.take() {
                        let desc = format!("{} to {}", m.source_name, m.target_name);
                        drop(m);
                        say(&speaker, &format!("Stopped duplicating {}", desc));
                    } else {
                        mirror = open_mirror_dialog(&nav, mode, &speaker, main_thread);
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
        } else if msg.message == mirror::WM_MIRROR_STOPPED && msg.hwnd.0.is_null() {
            if let Some(m) = mirror.take() {
                let reason = m.failure().unwrap_or_else(|| "unknown error".to_string());
                let target = m.target_name.clone();
                drop(m);
                say(
                    &speaker,
                    &format!("Duplication to {} stopped: {}", target, reason),
                );
            }
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
    }

    drop(mirror);
    drop(manager);
    drop(speaker);

    unsafe {
        CoUninitialize();
    }
}
