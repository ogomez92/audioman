//! The Shift+Enter "Duplicate audio" dialog: pick a source output, a target
//! output and a latency profile, then start the mirror engine.
//!
//! Same construction as the device picker (`dialog.rs`): plain Win32 combos
//! with a `STATIC` label created right before each one, so screen readers
//! name them natively, and the shared modal loop for Tab / Enter / Esc.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, RegisterClassExW, SetForegroundWindow,
    SetWindowLongPtrW, ShowWindow, CW_USEDEFAULT, GWLP_USERDATA, SW_SHOW, WNDCLASSEXW,
    WS_CAPTION, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_OVERLAPPED, WS_SYSMENU,
};

use crate::devices::{wide, DeviceInfo};
use crate::dialog::{
    fill_combo, make_button, make_combo, modal_wndproc, run_modal, selected_index, ModalState,
    ID_CANCEL, ID_OK,
};
use crate::mirror::Latency;

const ID_SOURCE: i32 = 1001;
const ID_TARGET: i32 = 1002;
const ID_LATENCY: i32 = 1003;

/// What the user picked. Indices refer to the `outputs` slice passed to
/// [`show`].
pub struct MirrorChoice {
    pub source: usize,
    pub target: usize,
    pub latency: Latency,
}

/// Show the modal dialog. `preselect_*` are indices into `outputs`. Returns
/// `None` if cancelled or the window could not be created.
pub fn show(
    outputs: &[DeviceInfo],
    preselect_source: usize,
    preselect_target: usize,
    preselect_latency: Latency,
) -> Option<MirrorChoice> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        let class_name = wide("AudiomanMirrorDialog");
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

        let title = wide("Duplicate audio");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            360,
            290,
            None,
            None,
            hinst,
            None,
        )
        .ok()?;

        // Label-then-combo order matters for screen-reader naming.
        let source_combo = make_combo(hwnd, hinst, ID_SOURCE, "Duplicate from:", 12)?;
        let target_combo = make_combo(hwnd, hinst, ID_TARGET, "Duplicate to:", 66)?;
        let latency_combo = make_combo(hwnd, hinst, ID_LATENCY, "Latency:", 120)?;
        let _ok_btn = make_button(hwnd, hinst, ID_OK, "Start", 178, 200)?;
        let cancel_btn = make_button(hwnd, hinst, ID_CANCEL, "Cancel", 262, 200)?;

        fill_combo(
            source_combo,
            outputs.iter().map(|d| d.name.as_str()),
            preselect_source,
        );
        fill_combo(
            target_combo,
            outputs.iter().map(|d| d.name.as_str()),
            preselect_target,
        );
        fill_combo(
            latency_combo,
            Latency::ALL.iter().map(|l| l.label()),
            Latency::ALL
                .iter()
                .position(|&l| l == preselect_latency)
                .unwrap_or(0),
        );

        let state_ptr = Box::into_raw(Box::new(ModalState::new(cancel_btn)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        // Land on the target: the source defaults to the device the user is
        // already on, so the target is the choice that usually matters.
        let _ = SetFocus(target_combo);

        run_modal(hwnd, state_ptr);

        let result = if (*state_ptr).accepted {
            let source = selected_index(source_combo);
            let target = selected_index(target_combo);
            let latency = selected_index(latency_combo)
                .and_then(|i| Latency::ALL.get(i).copied())
                .unwrap_or(Latency::Lowest);
            match (source, target) {
                (Some(source), Some(target))
                    if source < outputs.len() && target < outputs.len() =>
                {
                    Some(MirrorChoice {
                        source,
                        target,
                        latency,
                    })
                }
                _ => None,
            }
        } else {
            None
        };

        let _ = DestroyWindow(hwnd);
        drop(Box::from_raw(state_ptr));

        result
    }
}
