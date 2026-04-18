#![windows_subsystem = "windows"]

mod audio;
mod prism;

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_WIN, VK_BACK, VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, WM_HOTKEY};

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

const VOLUME_STEP: f32 = 0.02;

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
    let suffix = if new_v >= 1.0 {
        " maximum"
    } else if new_v <= 0.0 {
        " minimum"
    } else {
        ""
    };
    say(speaker, &format!("{} percent{}", pct, suffix));
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

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    let speaker = prism::Speaker::new();
    let manager = match audio::AudioManager::new() {
        Ok(m) => m,
        Err(e) => {
            say(&speaker, &format!("Audio manager failed to start: {}", e));
            return;
        }
    };
    let state = manager.state.clone();

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
                HK_PREV => navigate(&state, &speaker, -1),
                HK_NEXT => navigate(&state, &speaker, 1),
                HK_UP => apply_volume(&state, &speaker, |v| v + VOLUME_STEP),
                HK_DOWN => apply_volume(&state, &speaker, |v| v - VOLUME_STEP),
                HK_BIG_UP => apply_volume(&state, &speaker, |v| v + 0.20),
                HK_BIG_DOWN => apply_volume(&state, &speaker, |v| v - 0.20),
                HK_MIN => apply_volume(&state, &speaker, |_| 0.0),
                HK_MAX => apply_volume(&state, &speaker, |_| 1.0),
                HK_MUTE => toggle_mute(&state, &speaker),
                HK_RESET_ALL => {
                    let n = audio::restore_all_max(&state);
                    say(
                        &speaker,
                        &format!("Restored {} applications to 100 percent", n),
                    );
                }
                _ => {}
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
    }

    drop(manager);
    drop(speaker);

    unsafe {
        CoUninitialize();
    }
}
