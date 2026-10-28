//! The Shift+S "Stream to SonicRoom" dialog: server, room, the name shown in
//! the room, which device to stream, channels and quality.
//!
//! Same construction as the other dialogs (`dialog.rs`): plain Win32 edit
//! and combo controls, each named by the `STATIC` label created right before
//! it, and the shared modal loop for Tab / Enter / Esc.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW,
    ShowWindow, CW_USEDEFAULT, GWLP_USERDATA, SW_SHOW, WNDCLASSEXW, WS_CAPTION,
    WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_OVERLAPPED, WS_SYSMENU,
};

use crate::caster::{Channels, Quality, Settings, Source};
use crate::devices::wide;
use crate::dialog::{
    fill_combo, make_button, make_combo, make_edit, modal_wndproc, run_modal, selected_index,
    window_text, ModalState, ID_CANCEL, ID_OK,
};

const ID_SERVER: i32 = 1001;
const ID_ROOM: i32 = 1002;
const ID_NAME: i32 = 1003;
const ID_DEVICE: i32 = 1004;
const ID_CHANNELS: i32 = 1005;
const ID_QUALITY: i32 = 1006;

/// What the user entered. `device` indexes the `sources` slice passed to
/// [`show`]; the text fields are trimmed but not validated.
pub struct CastChoice {
    pub server: String,
    pub room: String,
    pub name: String,
    pub device: usize,
    pub channels: Channels,
    pub quality: Quality,
}

/// Show the modal dialog filled from `prefs`, with `preselect_device`
/// selected. Returns `None` if cancelled or the window could not be created.
pub fn show(sources: &[Source], preselect_device: usize, prefs: &Settings) -> Option<CastChoice> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        let class_name = wide("AudiomanCastDialog");
        // Re-registering fails harmlessly; the class lives for the process.
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(modal_wndproc),
            hInstance: hinst,
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                (windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 as isize + 1) as *mut c_void,
            ),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let title = wide("Stream to SonicRoom");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            360,
            440,
            None,
            None,
            hinst,
            None,
        )
        .ok()?;

        // Label-then-control order matters for screen-reader naming.
        let server = make_edit(hwnd, hinst, ID_SERVER, "Server or room link:", &prefs.server, 12)?;
        let room = make_edit(hwnd, hinst, ID_ROOM, "Room:", &prefs.room, 66)?;
        let name = make_edit(hwnd, hinst, ID_NAME, "Name in the room:", &prefs.name, 120)?;
        let device = make_combo(hwnd, hinst, ID_DEVICE, "Device to stream:", 174)?;
        let channels = make_combo(hwnd, hinst, ID_CHANNELS, "Channels:", 228)?;
        let quality = make_combo(hwnd, hinst, ID_QUALITY, "Quality:", 282)?;
        let _ok_btn = make_button(hwnd, hinst, ID_OK, "Start", 178, 350)?;
        let cancel_btn = make_button(hwnd, hinst, ID_CANCEL, "Cancel", 262, 350)?;

        let device_labels: Vec<String> = sources
            .iter()
            .map(|s| format!("{} ({})", s.name, if s.is_input { "input" } else { "output" }))
            .collect();
        fill_combo(device, device_labels.iter().map(String::as_str), preselect_device);
        fill_combo(
            channels,
            Channels::ALL.iter().map(|c| c.label()),
            Channels::ALL.iter().position(|&c| c == prefs.channels).unwrap_or(0),
        );
        fill_combo(
            quality,
            Quality::ALL.iter().map(|q| q.label()),
            Quality::ALL.iter().position(|&q| q == prefs.quality).unwrap_or(0),
        );

        let state_ptr = Box::into_raw(Box::new(ModalState::new(cancel_btn)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        // Start on the first thing still missing; with the room remembered
        // the device is usually the only choice left.
        let first: HWND = if prefs.server.is_empty() {
            server
        } else if prefs.room.is_empty() {
            room
        } else {
            device
        };
        let _ = SetFocus(first);

        run_modal(hwnd, state_ptr);

        let result = if (*state_ptr).accepted {
            selected_index(device)
                .filter(|&d| d < sources.len())
                .map(|device_idx| CastChoice {
                    server: window_text(server).trim().to_string(),
                    room: window_text(room).trim().to_string(),
                    name: window_text(name).trim().to_string(),
                    device: device_idx,
                    channels: selected_index(channels)
                        .and_then(|i| Channels::ALL.get(i).copied())
                        .unwrap_or(Channels::Auto),
                    quality: selected_index(quality)
                        .and_then(|i| Quality::ALL.get(i).copied())
                        .unwrap_or(Quality::Low),
                })
        } else {
            None
        };

        let _ = DestroyWindow(hwnd);
        drop(Box::from_raw(state_ptr));

        result
    }
}
