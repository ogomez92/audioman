#![windows_subsystem = "windows"]

mod audio;
mod devices;
mod dialog;
mod prism;
mod routing;

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_WIN, VK_BACK, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT,
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

const VOLUME_STEP: f32 = 0.02;

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
    if guard.sessions.is_empty() {
        drop(guard);
        say(speaker, "No audio applications");
        return;
    }
    let last = guard.sessions.len() - 1;
    let new_idx = match guard.current_index() {
        None => 0,
        Some(i) => {
            let i = i as i32 + step;
            i.clamp(0, last as i32) as usize
        }
    };
    guard.selection = Some(guard.sessions[new_idx].id);
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
/// survives re-enumeration.
struct DeviceNav {
    devices: Vec<devices::Endpoint>,
    selected: Option<Vec<u16>>,
}

impl DeviceNav {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            selected: None,
        }
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
            .or_else(|| self.devices.first());
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
    if nav.devices.is_empty() {
        say(speaker, "No audio devices");
        return;
    }
    let last = nav.devices.len() - 1;
    let new_idx = match nav.index() {
        None => 0,
        Some(i) => (i as i32 + step).clamp(0, last as i32) as usize,
    };
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
                                    &format!("Device mode, {}", device_description(e)),
                                ),
                                None => say(&speaker, "Device mode, no audio devices"),
                            }
                            Mode::Devices
                        }
                        Mode::Devices => {
                            let name = state.lock().unwrap().current().map(|s| s.name.clone());
                            match name {
                                Some(n) => say(&speaker, &format!("Application mode, {}", n)),
                                None => say(&speaker, "Application mode"),
                            }
                            Mode::Apps
                        }
                    };
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
    }

    drop(manager);
    drop(speaker);

    unsafe {
        CoUninitialize();
    }
}
