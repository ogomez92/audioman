//! A small, screen-reader-accessible device-picker dialog built directly from
//! Win32 controls.
//!
//! Accessibility comes from using the standard system control classes
//! (`COMBOBOX`, `BUTTON`) — they expose native MSAA/UIA roles, values and states
//! that NVDA / JAWS / Narrator read automatically. Each combo is named by the
//! `STATIC` label created immediately before it (the dialog manager's label
//! association heuristic). Keyboard handling (Tab/Shift+Tab between controls,
//! Enter = accept, Esc = cancel) is provided by `IsDialogMessageW` plus a little
//! explicit Enter/Esc handling, so the whole dialog is operable without a mouse.

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IsDialogMessageW, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, ShowWindow, TranslateMessage, CB_ADDSTRING, CB_GETCURSEL,
    CB_GETDROPPEDSTATE, CB_SETCURSEL, CW_USEDEFAULT, GWLP_USERDATA, HMENU, MSG, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_KEYDOWN, WM_SETFONT, WNDCLASSEXW,
    WS_CAPTION, WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME,
    WS_OVERLAPPED, WS_SYSMENU,
    WS_TABSTOP, WS_VISIBLE,
};

use crate::devices::{wide, DeviceInfo};

// Combo-box styles / scroll style aren't exposed as named constants by the
// `windows` crate, so define the raw bits we OR into the window style.
const CBS_DROPDOWNLIST: u32 = 0x0003;
const CBS_HASSTRINGS: u32 = 0x0200;
const WS_VSCROLL: u32 = 0x0020_0000;
const ES_AUTOHSCROLL: u32 = 0x0080;

// Control identifiers. OK/Cancel reuse the conventional dialog ids.
pub(crate) const ID_OK: i32 = 1;
pub(crate) const ID_CANCEL: i32 = 2;
const ID_INPUT: i32 = 1001;
const ID_OUTPUT: i32 = 1002;

// Each combo's first item is a synthetic "Default" entry; the real devices
// follow at list index 1.. (i.e. device N is at combo index N + 1).
const DEFAULT_ITEM_LABEL: &str = "Default";

/// One combo's outcome. `Default` means "follow the system default" — the
/// caller clears any per-app routing for that direction.
pub enum Choice {
    Default,
    Device { id: Vec<u16>, name: String },
}

/// The user's choices when the dialog is accepted.
pub struct DeviceSelection {
    pub input: Choice,
    pub output: Choice,
}

/// Window-proc-visible state shared by every modal window in this program,
/// kept on the heap with its pointer stashed in `GWLP_USERDATA`.
pub(crate) struct ModalState {
    /// Enter on this button cancels instead of accepting.
    pub cancel_btn: HWND,
    pub accepted: bool,
    pub done: bool,
}

impl ModalState {
    pub(crate) fn new(cancel_btn: HWND) -> Self {
        Self {
            cancel_btn,
            accepted: false,
            done: false,
        }
    }
}

/// Show the modal device picker for `app_name`. `preselect_*` are combo
/// indices (0 = the "Default" entry, device N at N + 1) so the dialog opens on
/// the app's current routing. Returns `Some(selection)` if accepted, `None` if
/// cancelled or on failure to create the window.
pub fn show(
    app_name: &str,
    inputs: &[DeviceInfo],
    preselect_input: usize,
    outputs: &[DeviceInfo],
    preselect_output: usize,
) -> Option<DeviceSelection> {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        let class_name = wide("AudiomanDeviceDialog");

        // Registering an already-registered class returns 0; that's fine — the
        // class persists for the process lifetime, so we just try every time.
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

        let title = wide(&format!("Audio devices for {}", app_name));
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            360,
            240,
            None,
            None,
            hinst,
            None,
        )
        .ok()?;

        // Create the controls. Order matters: each STATIC label precedes its
        // combo so screen readers adopt the label as the combo's name.
        let input_combo = make_combo(hwnd, hinst, ID_INPUT, "Input device:", 12)?;
        let output_combo = make_combo(hwnd, hinst, ID_OUTPUT, "Output device:", 66)?;
        let _ok_btn = make_button(hwnd, hinst, ID_OK, "OK", 178, 150)?;
        let cancel_btn = make_button(hwnd, hinst, ID_CANCEL, "Cancel", 262, 150)?;

        populate(input_combo, inputs, preselect_input);
        populate(output_combo, outputs, preselect_output);

        let state_ptr = Box::into_raw(Box::new(ModalState::new(cancel_btn)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(input_combo);

        run_modal(hwnd, state_ptr);

        let accepted = (*state_ptr).accepted;
        let result = if accepted {
            Some(DeviceSelection {
                input: resolve_selection(input_combo, inputs),
                output: resolve_selection(output_combo, outputs),
            })
        } else {
            None
        };

        // Tear down: destroy the window, then reclaim and drop the state box.
        let _ = DestroyWindow(hwnd);
        drop(Box::from_raw(state_ptr));

        result
    }
}

/// Run the nested modal message loop on the main thread until `state.done`.
/// Tab/Shift+Tab come from `IsDialogMessageW`; Enter accepts (unless focus is
/// on the Cancel button) and Esc cancels, except while a combo's dropdown is
/// open, where both act on the list instead.
pub(crate) unsafe fn run_modal(hwnd: HWND, state_ptr: *mut ModalState) {
    let mut msg = MSG::default();
    loop {
        if (*state_ptr).done {
            break;
        }
        if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
            break; // WM_QUIT
        }

        if msg.message == WM_KEYDOWN {
            let vk = msg.wParam.0 as u16;
            let focus = GetFocus();
            let dropped = SendMessageW(focus, CB_GETDROPPEDSTATE, WPARAM(0), LPARAM(0)).0 != 0;
            if !dropped && vk == VK_RETURN.0 {
                (*state_ptr).accepted = focus != (*state_ptr).cancel_btn;
                (*state_ptr).done = true;
                continue;
            }
            if !dropped && vk == VK_ESCAPE.0 {
                (*state_ptr).accepted = false;
                (*state_ptr).done = true;
                continue;
            }
        }

        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub(crate) unsafe fn make_combo(
    parent: HWND,
    hinstance: HINSTANCE,
    id: i32,
    label: &str,
    y: i32,
) -> Option<HWND> {
    make_label(parent, hinstance, label, y)?;
    let class = wide("COMBOBOX");
    let combo = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE(
            WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_VSCROLL | CBS_DROPDOWNLIST | CBS_HASSTRINGS,
        ),
        12,
        y + 18,
        330,
        220,
        parent,
        HMENU(id as isize as *mut c_void),
        hinstance,
        None,
    )
    .ok()?;
    set_font(combo);
    Some(combo)
}

/// A single-line text field named by the `STATIC` label created right
/// before it, like [`make_combo`].
pub(crate) unsafe fn make_edit(
    parent: HWND,
    hinstance: HINSTANCE,
    id: i32,
    label: &str,
    text: &str,
    y: i32,
) -> Option<HWND> {
    make_label(parent, hinstance, label, y)?;
    let class = wide("EDIT");
    let initial = wide(text);
    let edit = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        PCWSTR(class.as_ptr()),
        PCWSTR(initial.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL),
        12,
        y + 18,
        330,
        24,
        parent,
        HMENU(id as isize as *mut c_void),
        hinstance,
        None,
    )
    .ok()?;
    set_font(edit);
    Some(edit)
}

/// The text of an edit control (or any window).
pub(crate) unsafe fn window_text(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd).max(0) as usize;
    let mut buf = vec![0u16; len + 1];
    let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub(crate) unsafe fn make_label(
    parent: HWND,
    hinstance: HINSTANCE,
    text: &str,
    y: i32,
) -> Option<HWND> {
    let class = wide("STATIC");
    let caption = wide(text);
    let label = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class.as_ptr()),
        PCWSTR(caption.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
        12,
        y,
        330,
        16,
        parent,
        None,
        hinstance,
        None,
    )
    .ok()?;
    set_font(label);
    Some(label)
}

pub(crate) unsafe fn make_button(
    parent: HWND,
    hinstance: HINSTANCE,
    id: i32,
    text: &str,
    x: i32,
    y: i32,
) -> Option<HWND> {
    let class = wide("BUTTON");
    let caption = wide(text);
    let button = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class.as_ptr()),
        PCWSTR(caption.as_ptr()),
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0),
        x,
        y,
        80,
        26,
        parent,
        HMENU(id as isize as *mut c_void),
        hinstance,
        None,
    )
    .ok()?;
    set_font(button);
    Some(button)
}

pub(crate) unsafe fn set_font(control: HWND) {
    let font = GetStockObject(DEFAULT_GUI_FONT);
    SendMessageW(control, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
}

unsafe fn populate(combo: HWND, devices: &[DeviceInfo], preselect: usize) {
    // Item 0 is always "Default" (follow the system default); real devices
    // follow. The caller preselects the app's current routing, so opening the
    // dialog and pressing OK re-applies what's already in effect.
    let default_label = wide(DEFAULT_ITEM_LABEL);
    SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(default_label.as_ptr() as isize));
    for dev in devices {
        let text = wide(&dev.name);
        SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
    }
    let sel = if preselect <= devices.len() { preselect } else { 0 };
    SendMessageW(combo, CB_SETCURSEL, WPARAM(sel), LPARAM(0));
}

/// Fill a combo with plain `items` and select `preselect` (the first item
/// if out of range).
pub(crate) unsafe fn fill_combo<'a>(
    combo: HWND,
    items: impl Iterator<Item = &'a str>,
    preselect: usize,
) {
    let mut count = 0usize;
    for item in items {
        let text = wide(item);
        SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(text.as_ptr() as isize));
        count += 1;
    }
    let sel = if preselect < count { preselect } else { 0 };
    SendMessageW(combo, CB_SETCURSEL, WPARAM(sel), LPARAM(0));
}

pub(crate) unsafe fn selected_index(combo: HWND) -> Option<usize> {
    let idx = SendMessageW(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
    if idx < 0 {
        None
    } else {
        Some(idx as usize)
    }
}

/// Resolve a combo selection to a [`Choice`]. Index 0 is the "Default" entry;
/// index N maps to device N - 1.
unsafe fn resolve_selection(combo: HWND, devices: &[DeviceInfo]) -> Choice {
    match selected_index(combo) {
        Some(i) if i >= 1 => match devices.get(i - 1) {
            Some(d) => Choice::Device {
                id: d.id.clone(),
                name: d.name.clone(),
            },
            None => Choice::Default,
        },
        _ => Choice::Default,
    }
}

/// Window procedure shared by the modal windows: OK/Cancel buttons and the
/// close box finish the loop; everything else is default-handled.
pub(crate) unsafe extern "system" fn modal_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            if id == ID_OK || id == ID_CANCEL {
                finish(hwnd, id == ID_OK);
                return LRESULT(0);
            }
        }
        WM_CLOSE => {
            finish(hwnd, false);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

unsafe fn finish(hwnd: HWND, accepted: bool) {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ModalState;
    if !ptr.is_null() {
        (*ptr).accepted = accepted;
        (*ptr).done = true;
    }
}
