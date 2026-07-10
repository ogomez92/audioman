//! Per-application audio routing via the undocumented
//! `Windows.Media.Internal.AudioPolicyConfig` WinRT class — the mechanism
//! behind Settings → App volume and device preferences (and EarTrumpet).
//!
//! `SetPersistedDefaultAudioEndpoint(pid, flow, role, path)` persists a
//! default render/capture endpoint for the *application* that owns `pid`
//! (keyed by app identity, so it survives app restarts and reboots). Passing a
//! null HSTRING clears the assignment so the app follows the system default
//! again.
//!
//! The endpoint argument is not a bare MMDevice id — it must be wrapped as
//! `\\?\SWD#MMDEVAPI#<id>#{DEVINTERFACE_AUDIO_RENDER-or-CAPTURE}`.
//!
//! The factory interface changed IID at Windows 10 build 21390 without
//! changing layout; we try the current IID first and fall back to the older
//! one via a raw QueryInterface into the same wrapper (identical vtable).
//!
//! Both set and get fail with `E_INVALIDARG` when `pid` has no live audio
//! session — only call these with pids taken from enumerated sessions.

use std::ffi::c_void;

use windows::core::{Interface, GUID, HSTRING};
use windows::Win32::Media::Audio::{eCapture, eConsole, eMultimedia, eRender, EDataFlow};
use windows::Win32::System::WinRT::RoGetActivationFactory;

use factory::IAudioPolicyConfigFactory;

const CLASS_NAME: &str = "Windows.Media.Internal.AudioPolicyConfig";
const MMDEVAPI_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";
const RENDER_SUFFIX: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
const CAPTURE_SUFFIX: &str = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";

/// Pre-21H2 IID for the identical vtable.
const IID_DOWNLEVEL: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

/// Route `pid`'s audio (input or output stream) to `endpoint_id`, or back to
/// the system default when `endpoint_id` is `None`. Sets both the console and
/// multimedia roles, matching what Windows Settings does.
pub fn set_app_device(pid: u32, input: bool, endpoint_id: Option<&[u16]>) -> bool {
    if pid == 0 {
        return false; // no owning process to key the assignment on
    }
    let Some(factory) = acquire() else {
        return false;
    };
    let flow = flow_for(input);
    let path = endpoint_id.map(|id| endpoint_path(id, input));
    // HSTRING is a transparent handle; the null handle is the empty string,
    // which clears the persisted assignment.
    let raw: *mut c_void = match &path {
        Some(h) => unsafe { std::mem::transmute_copy(h) },
        None => std::ptr::null_mut(),
    };
    unsafe {
        [eConsole, eMultimedia].into_iter().all(|role| {
            factory
                .SetPersistedDefaultAudioEndpoint(pid, flow, role, raw)
                .is_ok()
        })
    }
}

/// The bare endpoint id currently persisted for `pid`, if the app has been
/// routed somewhere other than the system default.
pub fn app_device_id(pid: u32, input: bool) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let factory = acquire()?;
    unsafe {
        let mut raw: *mut c_void = std::ptr::null_mut();
        if factory
            .GetPersistedDefaultAudioEndpoint(pid, flow_for(input), eMultimedia, &mut raw)
            .is_err()
            || raw.is_null()
        {
            return None;
        }
        let path: HSTRING = std::mem::transmute(raw); // take ownership; Drop frees
        let path = path.to_string();
        let rest = path.strip_prefix(MMDEVAPI_PREFIX)?;
        let id = &rest[..rest.rfind('#')?];
        if id.is_empty() {
            None
        } else {
            Some(id.to_string())
        }
    }
}

fn flow_for(input: bool) -> EDataFlow {
    if input {
        eCapture
    } else {
        eRender
    }
}

fn acquire() -> Option<IAudioPolicyConfigFactory> {
    unsafe {
        let class = HSTRING::from(CLASS_NAME);
        let current: windows::core::Result<IAudioPolicyConfigFactory> =
            RoGetActivationFactory(&class);
        if let Ok(f) = current {
            return Some(f);
        }
        // Pre-21H2 Windows 10: same vtable behind the older IID.
        let unknown: windows::core::IUnknown = RoGetActivationFactory(&class).ok()?;
        let mut raw: *mut c_void = std::ptr::null_mut();
        if unknown.query(&IID_DOWNLEVEL, &mut raw).is_err() || raw.is_null() {
            return None;
        }
        Some(IAudioPolicyConfigFactory::from_raw(raw))
    }
}

fn endpoint_path(endpoint_id: &[u16], input: bool) -> HSTRING {
    let trimmed = match endpoint_id.split_last() {
        Some((0, rest)) => rest,
        _ => endpoint_id,
    };
    let id = String::from_utf16_lossy(trimmed);
    let suffix = if input { CAPTURE_SUFFIX } else { RENDER_SUFFIX };
    HSTRING::from(format!("{MMDEVAPI_PREFIX}{id}{suffix}"))
}

/// Minimal FFI for the AudioPolicyConfig factory. The interface is
/// IInspectable-based, so the first three methods are IInspectable's, followed
/// by 19 stubs we never call that exist only to land the three real methods at
/// the correct vtable offsets — **do not reorder or remove them**.
mod factory {
    #![allow(non_snake_case)]
    use std::ffi::c_void;
    use windows::core::{interface, IUnknown, IUnknown_Vtbl, HRESULT};
    use windows::Win32::Media::Audio::{EDataFlow, ERole};

    #[interface("ab3d4648-e242-459f-b02f-541c70306324")]
    pub unsafe trait IAudioPolicyConfigFactory: IUnknown {
        // IInspectable
        unsafe fn GetIids(&self, count: *mut u32, iids: *mut *mut c_void) -> HRESULT;
        unsafe fn GetRuntimeClassName(&self, name: *mut *mut c_void) -> HRESULT;
        unsafe fn GetTrustLevel(&self, level: *mut i32) -> HRESULT;
        // Unused slots
        unsafe fn add_CtxVolumeChange(&self) -> HRESULT;
        unsafe fn remove_CtxVolumeChanged(&self) -> HRESULT;
        unsafe fn add_RingerVibrateStateChanged(&self) -> HRESULT;
        unsafe fn remove_RingerVibrateStateChange(&self) -> HRESULT;
        unsafe fn SetVolumeGroupGainForId(&self) -> HRESULT;
        unsafe fn GetVolumeGroupGainForId(&self) -> HRESULT;
        unsafe fn GetActiveVolumeGroupForEndpointId(&self) -> HRESULT;
        unsafe fn GetVolumeGroupsForEndpoint(&self) -> HRESULT;
        unsafe fn GetCurrentVolumeContext(&self) -> HRESULT;
        unsafe fn SetVolumeGroupMuteForId(&self) -> HRESULT;
        unsafe fn GetVolumeGroupMuteForId(&self) -> HRESULT;
        unsafe fn SetRingerVibrateState(&self) -> HRESULT;
        unsafe fn GetRingerVibrateState(&self) -> HRESULT;
        unsafe fn SetPreferredChatApplication(&self) -> HRESULT;
        unsafe fn ResetPreferredChatApplication(&self) -> HRESULT;
        unsafe fn GetPreferredChatApplication(&self) -> HRESULT;
        unsafe fn GetCurrentChatApplications(&self) -> HRESULT;
        unsafe fn add_ChatContextChanged(&self) -> HRESULT;
        unsafe fn remove_ChatContextChanged(&self) -> HRESULT;
        // Real methods
        pub unsafe fn SetPersistedDefaultAudioEndpoint(
            &self,
            pid: u32,
            flow: EDataFlow,
            role: ERole,
            device_path: *mut c_void,
        ) -> HRESULT;
        pub unsafe fn GetPersistedDefaultAudioEndpoint(
            &self,
            pid: u32,
            flow: EDataFlow,
            role: ERole,
            device_path: *mut *mut c_void,
        ) -> HRESULT;
        pub unsafe fn ClearAllPersistedApplicationDefaultEndpoints(&self) -> HRESULT;
    }
}
