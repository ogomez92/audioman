//! The Shift+0 visibility dialog: a checkbox list of apps (application mode)
//! or devices (device mode), where unchecked entries are skipped by Left/Right
//! navigation.
//!
//! Built from a standard `SysListView32` with `LVS_EX_CHECKBOXES`, so screen
//! readers announce each row as a check box with its checked state, and Space
//! toggles it natively. Like the device picker, this is a registered-class
//! window rather than a real dialog: `IsDialogMessageW` supplies Tab/Shift+Tab,
//! and Enter = accept / Esc = cancel are handled explicitly.

use std::ffi::c_void;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LIST_VIEW_ITEM_STATE_FLAGS,
    LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED, LVIS_STATEIMAGEMASK, LVITEMW,
    LVM_GETITEMSTATE, LVM_INSERTCOLUMNW, LVM_INSERTITEMW, LVM_SETCOLUMNWIDTH,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVSCW_AUTOSIZE_USEHEADER, LVS_EX_CHECKBOXES,
    LVS_EX_FULLROWSELECT, LVS_NOCOLUMNHEADER, LVS_REPORT, LVS_SHOWSELALWAYS, LVS_SINGLESEL,
    WC_LISTVIEWW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, RegisterClassExW, SendMessageW, SetForegroundWindow,
    SetWindowLongPtrW, ShowWindow, CW_USEDEFAULT, GWLP_USERDATA, HMENU, SW_SHOW, WINDOW_STYLE,
    WNDCLASSEXW, WS_BORDER, WS_CAPTION,
    WS_CHILD, WS_EX_CLIENTEDGE, WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_OVERLAPPED,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use crate::devices::wide;
use crate::dialog::{
    make_button, make_label, modal_wndproc, run_modal, set_font, ModalState, ID_CANCEL, ID_OK,
};

const ID_LIST: i32 = 1001;

// Checkbox state lives in the state-image bits: image 1 = unchecked,
// image 2 = checked.
const STATE_UNCHECKED: u32 = 1 << 12;
const STATE_CHECKED: u32 = 2 << 12;

/// One row of the list.
pub struct AppEntry {
    pub name: String,
    pub shown: bool,
}

/// Show the modal visibility dialog for applications.
pub fn show(entries: &[AppEntry]) -> Option<Vec<bool>> {
    show_with("Applications to show", entries)
}

/// Show the modal visibility dialog for devices.
pub fn show_devices(entries: &[AppEntry]) -> Option<Vec<bool>> {
    show_with("Devices to show", entries)
}

/// Show the modal visibility dialog titled `title` (also used, with a colon,
/// as the list's label). Returns the `shown` flag for each entry (same order
/// as `entries`) if accepted, `None` if cancelled or on failure.
fn show_with(title: &str, entries: &[AppEntry]) -> Option<Vec<bool>> {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst: HINSTANCE = GetModuleHandleW(None).ok()?.into();
        let class_name = wide("AudiomanAppsDialog");
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

        let wtitle = wide(title);
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wtitle.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            360,
            370,
            None,
            None,
            hinst,
            None,
        )
        .ok()?;

        // Label first so screen readers adopt it as the list's name.
        make_label(hwnd, hinst, &format!("{}:", title), 12)?;
        let list = make_list(hwnd, hinst, title)?;
        let _ok_btn = make_button(hwnd, hinst, ID_OK, "OK", 178, 290)?;
        let cancel_btn = make_button(hwnd, hinst, ID_CANCEL, "Cancel", 262, 290)?;

        populate(list, entries);

        let state_ptr = Box::into_raw(Box::new(ModalState::new(cancel_btn)));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(list);

        run_modal(hwnd, state_ptr);

        let result = if (*state_ptr).accepted {
            Some((0..entries.len()).map(|i| is_checked(list, i)).collect())
        } else {
            None
        };

        let _ = DestroyWindow(hwnd);
        drop(Box::from_raw(state_ptr));

        result
    }
}

unsafe fn make_list(parent: HWND, hinstance: HINSTANCE, name: &str) -> Option<HWND> {
    let name = wide(name);
    let list = CreateWindowExW(
        WS_EX_CLIENTEDGE,
        WC_LISTVIEWW,
        PCWSTR(name.as_ptr()),
        WINDOW_STYLE(
            WS_CHILD.0
                | WS_VISIBLE.0
                | WS_TABSTOP.0
                | WS_BORDER.0
                | LVS_REPORT
                | LVS_NOCOLUMNHEADER
                | LVS_SINGLESEL
                | LVS_SHOWSELALWAYS,
        ),
        12,
        30,
        330,
        248,
        parent,
        HMENU(ID_LIST as isize as *mut c_void),
        hinstance,
        None,
    )
    .ok()?;
    set_font(list);
    let ex = (LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT) as usize;
    SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, WPARAM(ex), LPARAM(ex as isize));
    let col = LVCOLUMNW {
        mask: LVCF_WIDTH,
        cx: 300,
        ..Default::default()
    };
    SendMessageW(list, LVM_INSERTCOLUMNW, WPARAM(0), LPARAM(&col as *const _ as isize));
    Some(list)
}

unsafe fn populate(list: HWND, entries: &[AppEntry]) {
    for (i, entry) in entries.iter().enumerate() {
        let mut text = wide(&entry.name);
        let item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: i as i32,
            pszText: PWSTR(text.as_mut_ptr()),
            ..Default::default()
        };
        SendMessageW(list, LVM_INSERTITEMW, WPARAM(0), LPARAM(&item as *const _ as isize));
        let check = if entry.shown {
            STATE_CHECKED
        } else {
            STATE_UNCHECKED
        };
        set_item_state(list, i, check, LVIS_STATEIMAGEMASK.0);
    }
    SendMessageW(
        list,
        LVM_SETCOLUMNWIDTH,
        WPARAM(0),
        LPARAM(LVSCW_AUTOSIZE_USEHEADER as isize),
    );
    // Focus + select the first row so the screen reader lands on an item.
    if !entries.is_empty() {
        let flags = LVIS_FOCUSED.0 | LVIS_SELECTED.0;
        set_item_state(list, 0, flags, flags);
    }
}

unsafe fn set_item_state(list: HWND, index: usize, state: u32, mask: u32) {
    let item = LVITEMW {
        state: LIST_VIEW_ITEM_STATE_FLAGS(state),
        stateMask: LIST_VIEW_ITEM_STATE_FLAGS(mask),
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        WPARAM(index),
        LPARAM(&item as *const _ as isize),
    );
}

unsafe fn is_checked(list: HWND, index: usize) -> bool {
    let state = SendMessageW(
        list,
        LVM_GETITEMSTATE,
        WPARAM(index),
        LPARAM(LVIS_STATEIMAGEMASK.0 as isize),
    )
    .0 as u32;
    state & LVIS_STATEIMAGEMASK.0 == STATE_CHECKED
}
