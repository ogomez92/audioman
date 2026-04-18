use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use windows::core::{implement, Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::Media::Audio::{
    eConsole, eRender, AudioSessionDisconnectReason, AudioSessionState,
    AudioSessionStateExpired, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionEvents, IAudioSessionEvents_Impl, IAudioSessionManager2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDeviceEnumerator,
    ISimpleAudioVolume, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Session {
    pub id: u64,
    pub name: String,
    pub pid: u32,
    pub volume: ISimpleAudioVolume,
    _control: IAudioSessionControl2,
    _events: IAudioSessionEvents,
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

pub struct AudioManager {
    pub state: Arc<Mutex<State>>,
    manager: IAudioSessionManager2,
    notifier: IAudioSessionNotification,
}

impl AudioManager {
    pub fn new() -> windows::core::Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None)?;

            let state = Arc::new(Mutex::new(State {
                sessions: Vec::new(),
                selection: None,
            }));

            let notifier: IAudioSessionNotification = SessionNotifier {
                state: state.clone(),
            }
            .into();
            manager.RegisterSessionNotification(&notifier)?;

            // Enumerate existing sessions — this also primes the notification pump
            // so OnSessionCreated starts firing for future sessions.
            let session_enum = manager.GetSessionEnumerator()?;
            let count = session_enum.GetCount()?;
            for i in 0..count {
                if let Ok(ctrl) = session_enum.GetSession(i) {
                    let _ = add_session(&state, &ctrl);
                }
            }

            Ok(Self {
                state,
                manager,
                notifier,
            })
        }
    }
}

impl Drop for AudioManager {
    fn drop(&mut self) {
        unsafe {
            let _ = self.manager.UnregisterSessionNotification(&self.notifier);
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
        // Avoid duplicates when a real process session is re-enumerated
        if pid != 0 && guard.sessions.iter().any(|s| s.pid == pid) {
            return Ok(());
        }
        guard.sessions.push(Session {
            id,
            name,
            pid,
            volume: vol,
            _control: ctrl2,
            _events: events,
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
