//! Audio endpoint device enumeration, default-device switching, endpoint
//! volume/mute, and endpoint enable/disable.
//!
//! Enumerates render (output) and capture (input) endpoints with their
//! friendly names, reports the current default for each data-flow, and drives
//! the undocumented `IPolicyConfig` interface on `CPolicyConfigClient` for the
//! operations that have no public Win32 API: switching the system default
//! endpoint (`SetDefaultEndpoint`) and enabling/disabling an endpoint
//! (`SetEndpointVisibility`). We declare just enough of the vtable to reach
//! those two methods; the preceding slots are declared only to keep the vtable
//! layout correct and are never called.

use std::ffi::c_void;

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE, DEVICE_STATE_ACTIVE,
    DEVICE_STATE_DISABLED,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};

/// One enumerated audio endpoint.
pub struct DeviceInfo {
    /// Null-terminated wide endpoint id (usable directly as a `PCWSTR`).
    pub id: Vec<u16>,
    /// Friendly display name, e.g. "Speakers (Realtek Audio)".
    pub name: String,
}

/// Active output (render) endpoints.
pub fn outputs() -> Vec<DeviceInfo> {
    enumerate(eRender)
}

/// Active input (capture) endpoints.
pub fn inputs() -> Vec<DeviceInfo> {
    enumerate(eCapture)
}

/// Id of the current default output endpoint (console role).
pub fn default_output_id() -> Option<Vec<u16>> {
    default_id(eRender)
}

/// Make `device_id` the default endpoint for every role (console, multimedia,
/// communications). Returns true if all roles were set successfully.
pub fn set_default(device_id: &[u16]) -> bool {
    unsafe {
        let policy: IPolicyConfig =
            match CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) {
                Ok(p) => p,
                Err(_) => return false,
            };
        let id = PCWSTR(device_id.as_ptr());
        let mut ok = true;
        for role in [eConsole, eMultimedia, eCommunications] {
            if policy.SetDefaultEndpoint(id, role).is_err() {
                ok = false;
            }
        }
        ok
    }
}

/// Enable or disable an endpoint (the same operation as the Sound control
/// panel's Disable/Enable). Disabled endpoints stay enumerable via
/// [`all_endpoints`] so they can be re-enabled.
pub fn set_endpoint_enabled(device_id: &[u16], enabled: bool) -> bool {
    unsafe {
        let policy: IPolicyConfig =
            match CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) {
                Ok(p) => p,
                Err(_) => return false,
            };
        policy
            .SetEndpointVisibility(PCWSTR(device_id.as_ptr()), i32::from(enabled))
            .is_ok()
    }
}

/// One endpoint as seen by device-mode navigation.
pub struct Endpoint {
    /// Null-terminated wide endpoint id.
    pub id: Vec<u16>,
    /// Friendly display name.
    pub name: String,
    /// Capture (true) or render (false).
    pub is_input: bool,
    /// False when the endpoint is disabled.
    pub enabled: bool,
}

/// Every render and capture endpoint that is active *or disabled* (disabled
/// ones are listed so they can be re-enabled). Outputs first, then inputs.
pub fn all_endpoints() -> Vec<Endpoint> {
    let mut out = Vec::new();
    collect_endpoints(eRender, false, &mut out);
    collect_endpoints(eCapture, true, &mut out);
    out
}

fn collect_endpoints(flow: EDataFlow, is_input: bool, out: &mut Vec<Endpoint>) {
    unsafe {
        let Ok(enumerator) = enumerator() else { return };
        let mask = DEVICE_STATE(DEVICE_STATE_ACTIVE.0 | DEVICE_STATE_DISABLED.0);
        let Ok(collection) = enumerator.EnumAudioEndpoints(flow, mask) else {
            return;
        };
        let count = collection.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(device) = collection.Item(i) else { continue };
            let Ok(id_ptr) = device.GetId() else { continue };
            let id = pwstr_to_vec(id_ptr);
            CoTaskMemFree(Some(id_ptr.0 as *const c_void));
            let name = friendly_name(&device).unwrap_or_else(|| "Unknown device".to_string());
            let enabled = device
                .GetState()
                .map(|s| s.0 & DEVICE_STATE_ACTIVE.0 != 0)
                .unwrap_or(false);
            out.push(Endpoint {
                id,
                name,
                is_input,
                enabled,
            });
        }
    }
}

/// Master volume of an endpoint, 0.0..=1.0. `None` when the endpoint can't be
/// activated (e.g. it is disabled).
pub fn endpoint_volume(device_id: &[u16]) -> Option<f32> {
    unsafe { activate_volume(device_id)?.GetMasterVolumeLevelScalar().ok() }
}

pub fn set_endpoint_volume(device_id: &[u16], level: f32) -> bool {
    let Some(vol) = activate_volume(device_id) else {
        return false;
    };
    unsafe {
        vol.SetMasterVolumeLevelScalar(level.clamp(0.0, 1.0), &GUID::zeroed())
            .is_ok()
    }
}

pub fn endpoint_mute(device_id: &[u16]) -> Option<bool> {
    unsafe { activate_volume(device_id)?.GetMute().ok().map(|b| b.as_bool()) }
}

pub fn set_endpoint_mute(device_id: &[u16], mute: bool) -> bool {
    let Some(vol) = activate_volume(device_id) else {
        return false;
    };
    unsafe { vol.SetMute(mute, &GUID::zeroed()).is_ok() }
}

fn activate_volume(device_id: &[u16]) -> Option<IAudioEndpointVolume> {
    unsafe {
        let device = enumerator().ok()?.GetDevice(PCWSTR(device_id.as_ptr())).ok()?;
        device.Activate(CLSCTX_ALL, None).ok()
    }
}

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// Build a null-terminated UTF-16 buffer from a Rust string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Render a null-terminated wide id as a `String` (for comparisons against ids
/// returned by the routing APIs).
pub fn id_string(id: &[u16]) -> String {
    let trimmed = match id.split_last() {
        Some((0, rest)) => rest,
        _ => id,
    };
    String::from_utf16_lossy(trimmed)
}

fn enumerate(flow: EDataFlow) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                Ok(e) => e,
                Err(_) => return out,
            };
        let Ok(collection) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
            return out;
        };
        let count = collection.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(device) = collection.Item(i) else { continue };
            let Ok(id_ptr) = device.GetId() else { continue };
            let id = pwstr_to_vec(id_ptr);
            CoTaskMemFree(Some(id_ptr.0 as *const c_void));
            let name = friendly_name(&device).unwrap_or_else(|| "Unknown device".to_string());
            out.push(DeviceInfo { id, name });
        }
    }
    out
}

fn default_id(flow: EDataFlow) -> Option<Vec<u16>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let device = enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok()?;
        let id_ptr = device.GetId().ok()?;
        let id = pwstr_to_vec(id_ptr);
        CoTaskMemFree(Some(id_ptr.0 as *const c_void));
        Some(id)
    }
}

unsafe fn friendly_name(device: &IMMDevice) -> Option<String> {
    let store = device.OpenPropertyStore(STGM_READ).ok()?;
    let prop = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    let str_ptr = PropVariantToStringAlloc(&prop).ok()?;
    let name = str_ptr.to_string().ok();
    CoTaskMemFree(Some(str_ptr.0 as *const c_void));
    // `prop` is an owned PROPVARIANT and clears itself on drop.
    name
}

/// Copy a COM-allocated wide string into an owned, null-terminated buffer.
unsafe fn pwstr_to_vec(p: PWSTR) -> Vec<u16> {
    if p.0.is_null() {
        return vec![0];
    }
    let mut len = 0usize;
    while *p.0.add(len) != 0 {
        len += 1;
    }
    let mut v = Vec::with_capacity(len + 1);
    v.extend_from_slice(std::slice::from_raw_parts(p.0, len));
    v.push(0);
    v
}

const CLSID_POLICY_CONFIG_CLIENT: GUID =
    GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

use policy::IPolicyConfig;

/// Undocumented `IPolicyConfig` on `CPolicyConfigClient`. Only
/// `SetDefaultEndpoint` and `SetEndpointVisibility` are ever called; the other
/// entries exist solely to place them at the correct vtable slots. Wrapped in a
/// module so the deliberately PascalCase COM method names don't trip the
/// snake-case lint.
mod policy {
    #![allow(non_snake_case)]
    use super::ERole;
    use std::ffi::c_void;
    use windows::core::{interface, IUnknown, IUnknown_Vtbl, HRESULT, PCWSTR};

    #[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    pub unsafe trait IPolicyConfig: IUnknown {
        unsafe fn GetMixFormat(&self, name: PCWSTR, format: *mut *mut c_void) -> HRESULT;
        unsafe fn GetDeviceFormat(
            &self,
            name: PCWSTR,
            default: i32,
            format: *mut *mut c_void,
        ) -> HRESULT;
        unsafe fn ResetDeviceFormat(&self, name: PCWSTR) -> HRESULT;
        unsafe fn SetDeviceFormat(
            &self,
            name: PCWSTR,
            endpoint_fmt: *mut c_void,
            mix_fmt: *mut c_void,
        ) -> HRESULT;
        unsafe fn GetProcessingPeriod(
            &self,
            name: PCWSTR,
            default: i32,
            default_period: *mut i64,
            min_period: *mut i64,
        ) -> HRESULT;
        unsafe fn SetProcessingPeriod(&self, name: PCWSTR, period: *mut i64) -> HRESULT;
        unsafe fn GetShareMode(&self, name: PCWSTR, mode: *mut c_void) -> HRESULT;
        unsafe fn SetShareMode(&self, name: PCWSTR, mode: *mut c_void) -> HRESULT;
        unsafe fn GetPropertyValue(
            &self,
            name: PCWSTR,
            key: *const c_void,
            value: *mut c_void,
        ) -> HRESULT;
        unsafe fn SetPropertyValue(
            &self,
            name: PCWSTR,
            key: *const c_void,
            value: *mut c_void,
        ) -> HRESULT;
        pub unsafe fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
        pub unsafe fn SetEndpointVisibility(&self, name: PCWSTR, visible: i32) -> HRESULT;
    }
}
