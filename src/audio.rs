//! WASAPI session tracking across every active render endpoint.
//!
//! Sessions are enumerated on *all* active output devices (not just the
//! default) so per-app routing and default-device switches never lose track of
//! an application. An `IMMNotificationClient` watches for device changes
//! (added / removed / enabled / disabled / default switched) and posts
//! [`WM_DEVICES_CHANGED`] to the main thread, which responds by calling
//! [`AudioManager::rebuild`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{implement, Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{
    eRender, AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateExpired,
    EDataFlow, ERole, IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents,
    IAudioSessionEvents_Impl, IAudioSessionManager2, IAudioSessionNotification,
    IAudioSessionNotification_Impl, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, ISimpleAudioVolume, MMDeviceEnumerator, DEVICE_STATE,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

/// Thread message posted to the main thread whenever the set of audio devices
/// changes; the main loop responds by calling [`AudioManager::rebuild`].
pub const WM_DEVICES_CHANGED: u32 = WM_APP + 1;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Session {
    pub id: u64,
    pub name: String,
    pub pid: u32,
    pub volume: ISimpleAudioVolume,
    control: IAudioSessionControl2,
    events: IAudioSessionEvents,
}

pub struct State {
    pub sessions: Vec<Session>,
    pub selection: Option<u64>,
}

impl State {
    pub fn current_index(&self) -> Option<usize> {
        self.selection
            .and_then(|id| self.sessions.iter().position(|s| s.id == id))
    }

    pub fn current(&self) -> Option<&Session> {
        self.current_index().and_then(|i| self.sessions.get(i))
    }
}

/// One session manager + notifier per active render endpoint.
struct Attachment {
    manager: IAudioSessionManager2,
    notifier: IAudioSessionNotification,
}

pub struct AudioManager {
    pub state: Arc<Mutex<State>>,
    enumerator: IMMDeviceEnumerator,
    watcher: IMMNotificationClient,
    attachments: Vec<Attachment>,
}

impl AudioManager {
    /// `main_thread_id` receives [`WM_DEVICES_CHANGED`] when devices change.
    pub fn new(main_thread_id: u32) -> windows::core::Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;

            let state = Arc::new(Mutex::new(State {
                sessions: Vec::new(),
                selection: None,
            }));

            let watcher: IMMNotificationClient = DeviceWatcher {
                thread_id: main_thread_id,
            }
            .into();
            // Best-effort: without it we just miss hot-plug rebuilds.
            let _ = enumerator.RegisterEndpointNotificationCallback(&watcher);

            let mut mgr = Self {
                state,
                enumerator,
                watcher,
                attachments: Vec::new(),
            };
            mgr.attach_all();
            Ok(mgr)
        }
    }

    /// Re-enumerate devices and sessions, preserving the current selection by
    /// pid (or name, if the process re-appeared under a new session).
    pub fn rebuild(&mut self) {
        let previous = {
            let guard = self.state.lock().unwrap();
            guard.current().map(|s| (s.pid, s.name.clone()))
        };
        self.detach_all();
        self.attach_all();
        let mut guard = self.state.lock().unwrap();
        let restored = previous.and_then(|(pid, name)| {
            guard
                .sessions
                .iter()
                .find(|s| s.pid == pid)
                .or_else(|| guard.sessions.iter().find(|s| s.name == name))
                .map(|s| s.id)
        });
        guard.selection = restored;
    }

    /// Attach a session manager to every active render endpoint and enumerate
    /// its sessions — enumeration also primes the notification pump so
    /// `OnSessionCreated` fires for future sessions on that endpoint.
    fn attach_all(&mut self) {
        unsafe {
            let Ok(collection) = self
                .enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            else {
                return;
            };
            let count = collection.GetCount().unwrap_or(0);
            for i in 0..count {
                let Ok(device) = collection.Item(i) else { continue };
                let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)
                else {
                    continue;
                };
                let notifier: IAudioSessionNotification = SessionNotifier {
                    state: self.state.clone(),
                }
                .into();
                if manager.RegisterSessionNotification(&notifier).is_err() {
                    continue;
                }
                if let Ok(session_enum) = manager.GetSessionEnumerator() {
                    let n = session_enum.GetCount().unwrap_or(0);
                    for j in 0..n {
                        if let Ok(ctrl) = session_enum.GetSession(j) {
                            let _ = add_session(&self.state, &ctrl);
                        }
                    }
                }
                self.attachments.push(Attachment { manager, notifier });
            }
        }
    }

    fn detach_all(&mut self) {
        unsafe {
            for a in self.attachments.drain(..) {
                let _ = a.manager.UnregisterSessionNotification(&a.notifier);
            }
            let mut guard = self.state.lock().unwrap();
            for s in guard.sessions.drain(..) {
                let _ = s.control.UnregisterAudioSessionNotification(&s.events);
            }
            guard.selection = None;
        }
    }
}

impl Drop for AudioManager {
    fn drop(&mut self) {
        self.detach_all();
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.watcher);
        }
    }
}

fn add_session(state: &Arc<Mutex<State>>, ctrl: &IAudioSessionControl) -> windows::core::Result<()> {
    unsafe {
        let ctrl2: IAudioSessionControl2 = ctrl.cast()?;
        let vol: ISimpleAudioVolume = ctrl.cast()?;
        let pid = ctrl2.GetProcessId().unwrap_or(0);
        let name = if pid == 0 {
            "System sounds".to_string()
        } else {
            process_name(pid).unwrap_or_else(|| format!("Process {}", pid))
        };

        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);

        let events: IAudioSessionEvents = SessionEvents {
            id,
            state: state.clone(),
        }
        .into();
        ctrl.RegisterAudioSessionNotification(&events)?;

        let mut guard = state.lock().unwrap();
        // Every endpoint hosts a system-sounds session; keep a single entry.
        if pid == 0 && guard.sessions.iter().any(|s| s.pid == 0) {
            return Ok(());
        }
        if pid != 0 {
            // An app that moves endpoints creates its new session before the
            // old one expires — replace rather than skip, so the app never
            // vanishes from the list. (The replaced session's notification is
            // not unregistered here: we may be inside a WASAPI callback, where
            // unregistering can deadlock. The stale registration only delivers
            // events for an id we no longer track.)
            let was_selected = guard
                .sessions
                .iter()
                .any(|s| s.pid == pid && Some(s.id) == guard.selection);
            guard.sessions.retain(|s| s.pid != pid);
            if was_selected {
                guard.selection = Some(id);
            }
        }
        guard.sessions.push(Session {
            id,
            name,
            pid,
            volume: vol,
            control: ctrl2,
            events,
        });
    }
    Ok(())
}

fn remove_session(state: &Arc<Mutex<State>>, id: u64) {
    let mut guard = state.lock().unwrap();
    if guard.selection == Some(id) {
        guard.selection = None;
    }
    guard.sessions.retain(|s| s.id != id);
}

fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut size: u32 = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if result.is_err() || size == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..size as usize]);
        let stem = std::path::Path::new(&full)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&full)
            .to_string();
        Some(stem)
    }
}

pub fn get_volume(vol: &ISimpleAudioVolume) -> f32 {
    unsafe { vol.GetMasterVolume().unwrap_or(0.0) }
}

pub fn set_volume(vol: &ISimpleAudioVolume, level: f32) {
    let clamped = level.clamp(0.0, 1.0);
    let zero = GUID::zeroed();
    unsafe {
        let _ = vol.SetMasterVolume(clamped, &zero);
    }
}

pub fn get_mute(vol: &ISimpleAudioVolume) -> bool {
    unsafe { vol.GetMute().map(|b| b.as_bool()).unwrap_or(false) }
}

pub fn set_mute(vol: &ISimpleAudioVolume, mute: bool) {
    let zero = GUID::zeroed();
    unsafe {
        let _ = vol.SetMute(mute, &zero);
    }
}

pub fn restore_all_max(state: &Arc<Mutex<State>>) -> usize {
    let guard = state.lock().unwrap();
    let zero = GUID::zeroed();
    let mut n = 0usize;
    for s in &guard.sessions {
        unsafe {
            let _ = s.volume.SetMasterVolume(1.0, &zero);
            let _ = s.volume.SetMute(false, &zero);
        }
        n += 1;
    }
    n
}

/// Posts [`WM_DEVICES_CHANGED`] to the main thread on any device-set change.
/// Callbacks arrive on WASAPI worker threads, so no COM or state work happens
/// here — the main thread does the rebuild.
#[implement(IMMNotificationClient)]
struct DeviceWatcher {
    thread_id: u32,
}

impl DeviceWatcher {
    fn poke(&self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_DEVICES_CHANGED, WPARAM(0), LPARAM(0));
        }
    }
}

#[allow(non_snake_case)]
impl IMMNotificationClient_Impl for DeviceWatcher_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.poke();
        Ok(())
    }
    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        self.poke();
        Ok(())
    }
    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        self.poke();
        Ok(())
    }
    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        _role: ERole,
        _default_device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        self.poke();
        Ok(())
    }
    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[implement(IAudioSessionNotification)]
struct SessionNotifier {
    state: Arc<Mutex<State>>,
}

impl IAudioSessionNotification_Impl for SessionNotifier_Impl {
    fn OnSessionCreated(
        &self,
        new_session: Option<&IAudioSessionControl>,
    ) -> windows::core::Result<()> {
        if let Some(ctrl) = new_session {
            let _ = add_session(&self.state, ctrl);
        }
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct SessionEvents {
    id: u64,
    state: Arc<Mutex<State>>,
}

#[allow(non_snake_case)]
impl IAudioSessionEvents_Impl for SessionEvents_Impl {
    fn OnDisplayNameChanged(
        &self,
        _new_display_name: &PCWSTR,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(
        &self,
        _new_icon_path: &PCWSTR,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(
        &self,
        _new_volume: f32,
        _new_mute: BOOL,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnChannelVolumeChanged(
        &self,
        _channel_count: u32,
        _new_channel_volume_array: *const f32,
        _changed_channel: u32,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(
        &self,
        _new_grouping_param: *const GUID,
        _event_context: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, new_state: AudioSessionState) -> windows::core::Result<()> {
        if new_state == AudioSessionStateExpired {
            remove_session(&self.state, self.id);
        }
        Ok(())
    }
    fn OnSessionDisconnected(
        &self,
        _disconnect_reason: AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        remove_session(&self.state, self.id);
        Ok(())
    }
}
