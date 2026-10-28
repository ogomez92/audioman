//! Streaming one audio device live into a SonicRoom room.
//!
//! SonicRoom (WebRTC conferencing on a mediasoup SFU) accepts send-only
//! "caster" peers: they join with `role: "caster"`, skip the knock gate, are
//! never counted as people, and their single `source: "music"` producer is
//! announced and played to everyone in stereo (and ducked while people talk,
//! like any music). That is exactly a device being cast, so that is what we
//! join as. Anyone in the room can remove a caster; the server then
//! disconnects us on purpose and we stay gone ([`Phase::Removed`]).
//!
//! One worker thread (MTA COM, MMCSS "Pro Audio") does everything:
//!
//! 1. **Capture.** An output device is captured by WASAPI *loopback*, clocked
//!    by a silent render stream on the same device (loopback never signals on
//!    its own — see `mirror.rs`); an input device by an ordinary event-driven
//!    capture stream. Both ask for the engine's minimum period. When the mix
//!    format is already 48 kHz float with one or two channels the samples are
//!    used as-is; otherwise the engine converts to 48 kHz float stereo.
//! 2. **Encode.** Every [`Quality::frame_samples`] frames become one Opus
//!    packet. The low-delay presets use libopus's restricted-low-delay mode.
//! 3. **Send.** str0m (sans-IO WebRTC) does ICE / DTLS / SRTP over one UDP
//!    socket; [`crate::mediasoup`] fabricates the SDP and RTP parameters the
//!    SFU needs, and [`crate::socketio`] carries SonicRoom's signaling.
//!
//! The thread waits on the capture event, the socket's read event and the
//! stop event at once, so a packet leaves as soon as its last sample arrives.
//! Each change of [`Phase`] posts [`WM_CASTER_CHANGED`] to the main thread.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
};
use windows::Win32::Networking::WinSock::{
    WSACloseEvent, WSACreateEvent, WSAEventSelect, WSAResetEvent, FD_READ, SOCKET, WSAEVENT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
    WaitForMultipleObjects,
};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

use crate::devices::{self, id_string};
use crate::hidden::config_path;
use crate::mediasoup;
use crate::opus;
use crate::socketio::{self, Closed};
use crate::wasapi::{describe, fill_silence, open_stream, Format, Stream};

/// Posted to the main thread whenever the stream's [`Phase`] changes.
pub const WM_CASTER_CHANGED: u32 = WM_APP + 3;

/// How long ICE + DTLS may take before we give up on the room's media server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a signaling request may wait for its acknowledgement.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest wait between loop turns when nothing else wakes us.
const MAX_WAIT: Duration = Duration::from_millis(20);
/// No audio from the device for this long means its engine died.
const AUDIO_STALL: Duration = Duration::from_secs(2);

/// Packet size and codec mode: the trade between delay and quality.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quality {
    /// 5 ms packets, low-delay Opus.
    Lowest,
    /// 10 ms packets (what SonicRoom's own clients send), low-delay Opus.
    Low,
    /// 20 ms packets, full Opus at the room's top bitrate.
    High,
}

impl Quality {
    pub const ALL: [Quality; 3] = [Quality::Lowest, Quality::Low, Quality::High];

    /// Combo-box label.
    pub fn label(self) -> &'static str {
        match self {
            Quality::Lowest => "Lowest delay (5 millisecond packets)",
            Quality::Low => "Low delay (10 millisecond packets)",
            Quality::High => "High quality (20 millisecond packets, more delay)",
        }
    }

    /// Short spoken name.
    pub fn name(self) -> &'static str {
        match self {
            Quality::Lowest => "lowest delay",
            Quality::Low => "low delay",
            Quality::High => "high quality",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Quality::Lowest => "lowest",
            Quality::Low => "low",
            Quality::High => "high",
        }
    }

    fn parse(s: &str) -> Option<Quality> {
        Quality::ALL.into_iter().find(|q| q.key() == s.trim())
    }

    /// Samples per channel in one Opus packet at 48 kHz.
    pub fn frame_samples(self) -> usize {
        match self {
            Quality::Lowest => 240,
            Quality::Low => 480,
            Quality::High => 960,
        }
    }

    fn low_delay_mode(self) -> bool {
        self != Quality::High
    }

    /// Opus bitrate. The room's ceiling is 256 kbps.
    fn bitrate(self, stereo: bool) -> u32 {
        let stereo_rate = match self {
            Quality::Lowest | Quality::Low => 160_000,
            Quality::High => 256_000,
        };
        if stereo {
            stereo_rate
        } else {
            stereo_rate / 2
        }
    }
}

/// Mono or stereo. Automatic streams outputs in stereo (music, games) and
/// inputs in mono (almost every microphone is mono, and stereo would only
/// double the bandwidth every listener pays).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channels {
    Auto,
    Stereo,
    Mono,
}

impl Channels {
    pub const ALL: [Channels; 3] = [Channels::Auto, Channels::Stereo, Channels::Mono];

    pub fn label(self) -> &'static str {
        match self {
            Channels::Auto => "Automatic (stereo for outputs, mono for inputs)",
            Channels::Stereo => "Stereo",
            Channels::Mono => "Mono",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Channels::Auto => "auto",
            Channels::Stereo => "stereo",
            Channels::Mono => "mono",
        }
    }

    fn parse(s: &str) -> Option<Channels> {
        Channels::ALL.into_iter().find(|c| c.key() == s.trim())
    }

    pub fn stereo(self, is_input: bool) -> bool {
        match self {
            Channels::Auto => !is_input,
            Channels::Stereo => true,
            Channels::Mono => false,
        }
    }
}

/// A device that can be streamed.
pub struct Source {
    pub id: Vec<u16>,
    pub name: String,
    pub is_input: bool,
}

/// Every active device: outputs first, then inputs.
pub fn sources() -> Vec<Source> {
    let outputs = devices::outputs().into_iter().map(|d| Source {
        id: d.id,
        name: d.name,
        is_input: false,
    });
    let inputs = devices::inputs().into_iter().map(|d| Source {
        id: d.id,
        name: d.name,
        is_input: true,
    });
    outputs.chain(inputs).collect()
}

/// Whether SonicRoom will accept `room` as a room name.
pub fn valid_room(room: &str) -> bool {
    (1..=64).contains(&room.len())
        && room
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Everything one stream needs. Persisted twice, as `key=value` lines in
/// `%APPDATA%\audioman`: `sonicroom.txt` holds the last dialog choices, and
/// `active_sonicroom.txt` the stream the user wants running. The latter is
/// written once a stream first goes live and removed only when the user
/// stops it (or the room removes it); quitting, a lost connection or a
/// missing device keep it so the stream resumes.
#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub server: String,
    pub room: String,
    pub name: String,
    /// Endpoint id (`devices::id_string`).
    pub device_id: String,
    /// Spoken while the device is missing.
    pub device_name: String,
    pub channels: Channels,
    pub quality: Quality,
}

const PREFS_FILE: &str = "sonicroom.txt";
const ACTIVE_FILE: &str = "active_sonicroom.txt";

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            server: String::new(),
            room: String::new(),
            name: std::env::var("USERNAME").unwrap_or_else(|_| "audioman".to_string()),
            device_id: String::new(),
            device_name: String::new(),
            channels: Channels::Auto,
            quality: Quality::Low,
        }
    }
}

impl Settings {
    /// Last dialog choices, or defaults.
    pub fn load_prefs() -> Settings {
        read_file(PREFS_FILE)
            .map(|t| Settings::from_text(&t))
            .unwrap_or_default()
    }

    pub fn save_prefs(&self) {
        write_file(PREFS_FILE, &self.to_text());
    }

    /// The stream wanted running, if any.
    pub fn load_active() -> Option<Settings> {
        let s = Settings::from_text(&read_file(ACTIVE_FILE)?);
        (!s.server.is_empty() && valid_room(&s.room) && !s.device_id.is_empty()).then_some(s)
    }

    pub fn save_active(&self) {
        write_file(ACTIVE_FILE, &self.to_text());
    }

    pub fn clear_active() {
        if let Some(p) = config_path(ACTIVE_FILE) {
            let _ = std::fs::remove_file(p);
        }
    }

    fn to_text(&self) -> String {
        format!(
            "server={}\r\nroom={}\r\nname={}\r\ndevice={}\r\ndevice_name={}\r\nchannels={}\r\nquality={}\r\n",
            self.server,
            self.room,
            self.name,
            self.device_id,
            self.device_name,
            self.channels.key(),
            self.quality.key()
        )
    }

    /// Unknown keys and unparsable values fall back to the defaults.
    fn from_text(text: &str) -> Settings {
        let mut s = Settings::default();
        for (key, value) in text.lines().filter_map(|l| l.split_once('=')) {
            let value = value.trim();
            match key.trim() {
                "server" => s.server = value.to_string(),
                "room" => s.room = value.to_string(),
                "name" if !value.is_empty() => s.name = value.to_string(),
                "device" => s.device_id = value.to_string(),
                "device_name" => s.device_name = value.to_string(),
                "channels" => s.channels = Channels::parse(value).unwrap_or(s.channels),
                "quality" => s.quality = Quality::parse(value).unwrap_or(s.quality),
                _ => {}
            }
        }
        s
    }
}

fn read_file(name: &str) -> Option<String> {
    std::fs::read_to_string(config_path(name)?).ok()
}

fn write_file(name: &str, text: &str) {
    let Some(p) = config_path(name) else {
        return;
    };
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(p, text);
}

/// Where a stream is in its life.
#[derive(Clone, PartialEq, Debug)]
pub enum Phase {
    /// Signaling, ICE and DTLS in progress.
    Connecting,
    /// Audio is flowing into the room.
    Live,
    /// Stopped on its own: unreachable server, lost connection, device gone.
    Failed(String),
    /// Someone in the room removed the caster.
    Removed,
}

/// Engine facts, filled in by the worker.
#[derive(Default)]
pub struct Stats {
    /// Capture engine period in frames.
    pub capture_period: AtomicU32,
    /// Samples per channel per packet.
    pub frame_samples: AtomicU32,
    /// Opus packets handed to the network.
    pub packets: AtomicU64,
    /// Highest sequence number the SFU reported receiving (RTCP receiver
    /// report), so tests can tell audio actually arrived.
    pub received_seq: AtomicU64,
    /// Round trip to the SFU in milliseconds, from the same reports.
    pub rtt_ms: AtomicU32,
    /// Loudest sample sent so far, in thousandths of full scale.
    pub peak_milli: AtomicU32,
}

impl Stats {
    /// Delay added before the network: one capture period, one packet of
    /// samples and the encoder's look-ahead. The network and the listeners'
    /// jitter buffers come on top.
    pub fn estimated_delay_ms(&self, quality: Quality) -> u32 {
        let frames = self.capture_period.load(Ordering::Relaxed)
            + self.frame_samples.load(Ordering::Relaxed);
        let lookahead_us = if quality.low_delay_mode() { 2_500 } else { 6_500 };
        ((frames as u64 * 1_000_000 / opus::SAMPLE_RATE as u64 + lookahead_us) / 1000) as u32
    }
}

/// A running stream. Dropping it leaves the room and joins its thread.
pub struct Caster {
    stop_event: HANDLE,
    thread: Option<JoinHandle<()>>,
    phase: Arc<Mutex<Phase>>,
    pub stats: Arc<Stats>,
    pub settings: Settings,
}

// HANDLE is a raw pointer newtype; the event is only signalled by the owner
// and waited on by the worker.
unsafe impl Send for Caster {}

impl Caster {
    /// Start streaming `source` with `settings`. Blocks only until the device
    /// is open, so device errors come back here; connecting to the room
    /// happens on the worker and reports through [`Caster::phase`].
    pub fn start(settings: &Settings, source: &Source, main_thread: u32) -> Result<Caster, String> {
        let ws_url = socketio::websocket_url(&settings.server)?;
        if !valid_room(&settings.room) {
            return Err("room names use only letters, digits, - and _".to_string());
        }
        let stop_event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| format!("could not create event: {}", e))?;
        let phase = Arc::new(Mutex::new(Phase::Connecting));
        let stats = Arc::new(Stats::default());
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let mut settings = settings.clone();
        settings.device_id = id_string(&source.id);
        settings.device_name = source.name.clone();
        let cfg = Config {
            ws_url,
            room: settings.room.to_lowercase(),
            name: settings.name.clone(),
            device_id: source.id.clone(),
            device_name: source.name.clone(),
            is_input: source.is_input,
            stereo: settings.channels.stereo(source.is_input),
            quality: settings.quality,
            stats: stats.clone(),
        };
        let worker_phase = phase.clone();
        let stop_raw = stop_event.0 as usize;
        let thread = std::thread::Builder::new()
            .name("audioman-caster".into())
            .spawn(move || {
                let stop = HANDLE(stop_raw as *mut c_void);
                worker(cfg, stop, ready_tx, worker_phase, main_thread);
            })
            .map_err(|e| format!("could not start thread: {}", e))?;

        let ready = ready_rx
            .recv()
            .unwrap_or_else(|_| Err("streaming thread died during start-up".to_string()));
        match ready {
            Ok(()) => Ok(Caster {
                stop_event,
                thread: Some(thread),
                phase,
                stats,
                settings,
            }),
            Err(e) => {
                let _ = thread.join();
                unsafe {
                    let _ = CloseHandle(stop_event);
                }
                Err(e)
            }
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase.lock().unwrap().clone()
    }
}

impl Drop for Caster {
    fn drop(&mut self) {
        unsafe {
            let _ = SetEvent(self.stop_event);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = CloseHandle(self.stop_event);
        }
    }
}

struct Config {
    ws_url: String,
    room: String,
    name: String,
    device_id: Vec<u16>,
    device_name: String,
    is_input: bool,
    stereo: bool,
    quality: Quality,
    stats: Arc<Stats>,
}

/// How the streaming loop ended.
enum Ended {
    Stopped,
    Failed(String),
    Removed,
}

fn worker(
    cfg: Config,
    stop: HANDLE,
    ready: mpsc::Sender<Result<(), String>>,
    phase: Arc<Mutex<Phase>>,
    main_thread: u32,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let set_phase = |p: Phase| {
        *phase.lock().unwrap() = p;
        unsafe {
            let _ = PostThreadMessageW(
                main_thread,
                WM_CASTER_CHANGED,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    };

    match unsafe { Capture::open(&cfg) } {
        Err(e) => {
            let _ = ready.send(Err(e));
        }
        Ok(capture) => {
            let _ = ready.send(Ok(()));
            let mut task_index = 0u32;
            let mmcss =
                unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();

            let ended = match Session::connect(&cfg, stop) {
                Err(e) if stopped(stop) => {
                    let _ = e;
                    Ended::Stopped
                }
                Err(e) => Ended::Failed(e),
                Ok(mut session) => {
                    let ended = session.run(capture, stop, &cfg, &set_phase);
                    session.leave();
                    ended
                }
            };
            match ended {
                Ended::Stopped => {}
                Ended::Failed(e) => set_phase(Phase::Failed(e)),
                Ended::Removed => set_phase(Phase::Removed),
            }

            if let Some(h) = mmcss {
                unsafe {
                    let _ = AvRevertMmThreadCharacteristics(h);
                }
            }
        }
    }
    unsafe {
        CoUninitialize();
    }
}

fn stopped(stop: HANDLE) -> bool {
    unsafe { WaitForMultipleObjects(&[stop], false, 0) == WAIT_OBJECT_0 }
}

/// The device side: a capture stream plus, for loopback, the silent render
/// stream whose event clocks it.
struct Capture {
    client: IAudioClient,
    reader: IAudioCaptureClient,
    /// Loopback only: silent render stream on the same output.
    clock: Option<(Stream, IAudioRenderClient)>,
    /// Signalled once per engine period.
    event: HANDLE,
    /// Interleaved float channels the device hands us (1 or 2).
    channels: usize,
}

impl Capture {
    unsafe fn open(cfg: &Config) -> Result<Capture, String> {
        let what = if cfg.is_input {
            "input device"
        } else {
            "output device"
        };
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("device enumerator unavailable: {}", e))?;
        let device = enumerator
            .GetDevice(PCWSTR(cfg.device_id.as_ptr()))
            .map_err(|_| format!("{} not found", what))?;
        let event = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| format!("could not create event: {}", e))?;

        let result = (|| {
            let probe: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("{}: activation failed: {}", what, e))?;
            let mix = Format::mix_format(&probe)?;
            drop(probe);
            // Take the engine's own samples when they are already what Opus
            // wants; otherwise have the engine convert (resample / downmix).
            let direct =
                mix.is_float() && mix.rate() == opus::SAMPLE_RATE && mix.channels() <= 2;
            let (fmt, convert) = if direct {
                (Format { bytes: mix.bytes.clone() }, 0)
            } else {
                (
                    Format::float(opus::SAMPLE_RATE, mix.channels().clamp(1, 2)),
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                )
            };
            let channels = fmt.channels() as usize;

            if cfg.is_input {
                let stream = open_stream(&device, &fmt, convert, true, event, what)?;
                let reader: IAudioCaptureClient = stream
                    .client
                    .GetService()
                    .map_err(|e| format!("{}: capture service failed: {}", what, e))?;
                cfg.stats
                    .capture_period
                    .store(stream.period_frames, Ordering::Relaxed);
                return Ok(Capture {
                    client: stream.client,
                    reader,
                    clock: None,
                    event,
                    channels,
                });
            }

            let clock = open_stream(&device, &mix, 0, true, event, what)?;
            let clock_render: IAudioRenderClient = clock
                .client
                .GetService()
                .map_err(|e| format!("{}: render service failed: {}", what, e))?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("{}: activation failed: {}", what, e))?;
            // Generous buffer (200 ms): drained every clock tick, but a late
            // tick must not lose audio.
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | convert,
                    2_000_000,
                    0,
                    fmt.as_ptr(),
                    None,
                )
                .map_err(|e| format!("{}: loopback capture failed: {}", what, e))?;
            let reader: IAudioCaptureClient = client
                .GetService()
                .map_err(|e| format!("{}: capture service failed: {}", what, e))?;
            cfg.stats
                .capture_period
                .store(clock.period_frames, Ordering::Relaxed);
            Ok(Capture {
                client,
                reader,
                clock: Some((clock, clock_render)),
                event,
                channels,
            })
        })();
        if result.is_err() {
            let _ = CloseHandle(event);
        }
        result
    }

    unsafe fn start(&self) -> Result<(), String> {
        let fail = |e: windows::core::Error| format!("could not start the device: {}", e);
        self.client.Start().map_err(fail)?;
        if let Some((clock, render)) = &self.clock {
            fill_silence(&clock.client, render, clock.buffer_frames).map_err(fail)?;
            clock.client.Start().map_err(fail)?;
        }
        Ok(())
    }

    /// One engine period elapsed: move every pending packet into `out`,
    /// interleaved with `out_channels` channels.
    unsafe fn read(
        &mut self,
        out: &mut VecDeque<f32>,
        out_channels: usize,
    ) -> windows::core::Result<()> {
        if let Some((clock, render)) = &self.clock {
            fill_silence(&clock.client, render, clock.buffer_frames)?;
        }
        loop {
            if self.reader.GetNextPacketSize()? == 0 {
                return Ok(());
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            self.reader
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
            let samples: &[f32] = if silent {
                &[]
            } else {
                std::slice::from_raw_parts(data as *const f32, frames as usize * self.channels)
            };
            for f in 0..frames as usize {
                let frame = samples.get(f * self.channels..(f + 1) * self.channels);
                let (l, r) = match frame {
                    None => (0.0, 0.0),
                    Some([m]) => (*m, *m),
                    Some(s) => (s[0], s[1]),
                };
                if out_channels == 2 {
                    out.push_back(l);
                    out.push_back(r);
                } else {
                    out.push_back((l + r) * 0.5);
                }
            }
            self.reader.ReleaseBuffer(frames)?;
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            if let Some((clock, _)) = &self.clock {
                let _ = clock.client.Stop();
            }
            let _ = CloseHandle(self.event);
        }
    }
}

/// The network side: signaling, the WebRTC session and its socket.
struct Session {
    signaling: socketio::Client,
    rtc: Rtc,
    mid: Mid,
    socket: UdpSocket,
    local_addr: SocketAddr,
    /// Signalled by Winsock when the socket has data.
    socket_event: WSAEVENT,
}

impl Session {
    /// Join the room as a caster and set up (but not yet connect) the media
    /// path: create-transport, local offer / fabricated answer,
    /// connect-transport, produce.
    fn connect(cfg: &Config, stop: HANDLE) -> Result<Session, String> {
        let abort = || stopped(stop);
        let mut signaling = socketio::Client::connect(&cfg.ws_url, &abort)?;
        let mut request = |event: &str, data: Value| -> Result<Value, String> {
            let reply = signaling.request(event, data, REQUEST_TIMEOUT, &abort)?;
            if reply["ok"].as_bool() == Some(true) {
                Ok(reply)
            } else {
                Err(server_error(event, reply["error"].as_str().unwrap_or("refused")))
            }
        };

        request(
            "join",
            json!({ "roomName": cfg.room, "displayName": cfg.name, "role": "caster" }),
        )?;
        let reply = request("create-transport", json!({ "direction": "send" }))?;
        let mut server = mediasoup::TransportParams::from_json(&reply["params"])?;
        // One IPv4 socket: keep the candidates it can reach.
        server
            .candidates
            .retain(|c| c.ip.parse::<IpAddr>().is_ok_and(|ip| ip.is_ipv4()));
        let first = server
            .candidates
            .first()
            .ok_or("the room's audio server has no IPv4 address")?;
        let first: SocketAddr = format!("{}:{}", first.ip, first.port)
            .parse()
            .map_err(|_| "the room's audio server address is not valid".to_string())?;

        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("no UDP socket: {}", e))?;
        let local_addr = SocketAddr::new(route_to(first)?, socket.local_addr().map_err(io)?.port());

        let mut rtc = Rtc::builder()
            .clear_codecs()
            .enable_opus(true, false)
            .set_stats_interval(Some(Duration::from_secs(1)))
            .build(Instant::now());
        let host = Candidate::host(local_addr, "udp").map_err(|e| format!("bad address: {}", e))?;
        rtc.add_local_candidate(host);
        let mut api = rtc.sdp_api();
        let mid = api.add_media(
            MediaKind::Audio,
            Direction::SendOnly,
            Some("audioman".into()),
            Some("cast".into()),
            None,
        );
        let (offer, pending) = api.apply().ok_or("could not create the local offer")?;
        let local = mediasoup::parse_offer(&offer.to_sdp_string())?;
        let answer = SdpAnswer::from_sdp_string(&mediasoup::answer(&server, &local, cfg.stereo))
            .map_err(|e| format!("bad answer: {}", e))?;
        rtc.sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| format!("answer rejected: {}", e))?;

        request(
            "connect-transport",
            json!({ "direction": "send", "dtlsParameters": mediasoup::dtls_parameters(&local) }),
        )?;
        request(
            "produce",
            json!({
                "kind": "audio",
                "rtpParameters": mediasoup::rtp_parameters(&local, cfg.stereo),
                "source": "music",
                "title": cfg.device_name.chars().take(120).collect::<String>(),
            }),
        )?;

        let socket_event = unsafe {
            let ev = WSACreateEvent().map_err(|e| format!("no socket event: {}", e))?;
            // Also makes the socket non-blocking.
            if WSAEventSelect(SOCKET(socket.as_raw_socket() as usize), ev, FD_READ as i32) != 0 {
                let _ = WSACloseEvent(ev);
                return Err("could not watch the UDP socket".to_string());
            }
            ev
        };
        Ok(Session {
            signaling,
            rtc,
            mid,
            socket,
            local_addr,
            socket_event,
        })
    }

    /// Stream until stopped, failed or removed.
    fn run(
        &mut self,
        mut capture: Capture,
        stop: HANDLE,
        cfg: &Config,
        set_phase: &dyn Fn(Phase),
    ) -> Ended {
        match self.pump(&mut capture, stop, cfg, set_phase) {
            Ok(()) => Ended::Stopped,
            Err(Closed::ByServer) => Ended::Removed,
            Err(Closed::Lost(_)) if self.removed_by_server() => Ended::Removed,
            Err(Closed::Lost(e)) => Ended::Failed(e),
        }
    }

    /// Removing a caster closes its media path and its signaling at almost
    /// the same moment, and the media side may notice first. Give the
    /// server's disconnect a moment to arrive before calling it a failure.
    fn removed_by_server(&mut self) -> bool {
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.signaling.poll() {
                Err(Closed::ByServer) => return true,
                Err(Closed::Lost(_)) => return false,
                Ok(()) if Instant::now() >= deadline => return false,
                Ok(()) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    fn pump(
        &mut self,
        capture: &mut Capture,
        stop: HANDLE,
        cfg: &Config,
        set_phase: &dyn Fn(Phase),
    ) -> Result<(), Closed> {
        let lost = Closed::Lost;
        unsafe { capture.start() }.map_err(lost)?;
        let enc_channels = if cfg.stereo { 2 } else { 1 };
        let frame = cfg.quality.frame_samples();
        let mut encoder = opus::Encoder::new(
            enc_channels,
            cfg.quality.low_delay_mode(),
            cfg.quality.bitrate(cfg.stereo),
        )
        .map_err(lost)?;
        cfg.stats.frame_samples.store(frame as u32, Ordering::Relaxed);

        let socket_event = HANDLE(self.socket_event.0 as *mut c_void);
        let handles = [stop, capture.event, socket_event];
        let started = Instant::now();
        let mut live = false;
        let mut last_audio = Instant::now();
        let mut pending: VecDeque<f32> = VecDeque::new();
        let mut pcm = vec![0f32; frame * enc_channels];
        let mut packet = [0u8; opus::MAX_PACKET];
        let mut media_samples: u64 = 0;
        let mut buf = vec![0u8; 2000];

        loop {
            let deadline = self.drain(cfg, &mut live, set_phase)?;
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .min(MAX_WAIT);
            let signalled =
                unsafe { WaitForMultipleObjects(&handles, false, wait.as_millis() as u32) };
            match signalled.0.wrapping_sub(WAIT_OBJECT_0.0) {
                0 => return Ok(()),
                1 => {
                    last_audio = Instant::now();
                    unsafe { capture.read(&mut pending, enc_channels) }
                        .map_err(|e| Closed::Lost(describe(e)))?;
                    if !live {
                        // Not connected yet: nothing to send it to.
                        pending.clear();
                    }
                    while pending.len() >= pcm.len() {
                        for (dst, src) in pcm.iter_mut().zip(pending.drain(..frame * enc_channels)) {
                            *dst = src;
                        }
                        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
                        cfg.stats
                            .peak_milli
                            .fetch_max((peak.min(1.0) * 1000.0) as u32, Ordering::Relaxed);
                        let n = encoder.encode(&pcm, &mut packet).map_err(lost)?;
                        let time = MediaTime::new(media_samples, Frequency::FORTY_EIGHT_KHZ);
                        media_samples += frame as u64;
                        let writer = self.rtc.writer(self.mid).ok_or_else(|| {
                            Closed::Lost("the audio stream disappeared".to_string())
                        })?;
                        let pt = writer
                            .payload_params()
                            .next()
                            .map(|p| p.pt())
                            .ok_or_else(|| Closed::Lost("Opus was not negotiated".to_string()))?;
                        writer
                            .write(pt, Instant::now(), time, &packet[..n])
                            .map_err(rtc_error)?;
                        cfg.stats.packets.fetch_add(1, Ordering::Relaxed);
                        self.drain(cfg, &mut live, set_phase)?;
                    }
                }
                2 => {
                    unsafe {
                        let _ = WSAResetEvent(socket_event);
                    }
                    loop {
                        match self.socket.recv_from(&mut buf) {
                            Ok((n, source)) => {
                                let Ok(contents) = buf[..n].try_into() else {
                                    continue;
                                };
                                let input = Input::Receive(
                                    Instant::now(),
                                    Receive {
                                        proto: Protocol::Udp,
                                        source,
                                        destination: self.local_addr,
                                        contents,
                                    },
                                );
                                if self.rtc.accepts(&input) {
                                    self.rtc.handle_input(input).map_err(rtc_error)?;
                                    self.drain(cfg, &mut live, set_phase)?;
                                }
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            // ICMP "port unreachable" from an earlier send
                            // surfaces here on Windows; ICE deals with it.
                            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                            Err(_) => {
                                return Err(Closed::Lost(
                                    "lost the connection to the room".to_string(),
                                ))
                            }
                        }
                    }
                }
                _ if signalled == WAIT_TIMEOUT => {}
                _ => return Err(Closed::Lost("waiting for audio events failed".to_string())),
            }

            self.rtc
                .handle_input(Input::Timeout(Instant::now()))
                .map_err(rtc_error)?;
            self.signaling.poll()?;
            if !live && started.elapsed() > CONNECT_TIMEOUT {
                return Err(Closed::Lost(
                    "could not reach the room's audio server".to_string(),
                ));
            }
            if last_audio.elapsed() > AUDIO_STALL {
                return Err(Closed::Lost("the audio device stopped responding".to_string()));
            }
        }
    }

    /// Drain str0m until it only has a timeout left, which is returned.
    fn drain(
        &mut self,
        cfg: &Config,
        live: &mut bool,
        set_phase: &dyn Fn(Phase),
    ) -> Result<Instant, Closed> {
        loop {
            match self.rtc.poll_output().map_err(rtc_error)? {
                Output::Timeout(t) => return Ok(t),
                Output::Transmit(t) => {
                    // A full send buffer drops the packet, like any UDP loss.
                    let _ = self.socket.send_to(&t.contents, t.destination);
                }
                Output::Event(Event::Connected) => {
                    *live = true;
                    set_phase(Phase::Live);
                }
                Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => {
                    return Err(Closed::Lost(
                        "lost the connection to the room".to_string(),
                    ));
                }
                Output::Event(Event::MediaEgressStats(s)) => {
                    if let Some(rtt) = s.rtt {
                        cfg.stats
                            .rtt_ms
                            .store(rtt.as_millis() as u32, Ordering::Relaxed);
                    }
                    if let Some(remote) = s.remote {
                        cfg.stats
                            .received_seq
                            .store(*remote.maximum_sequence_number, Ordering::Relaxed);
                    }
                }
                Output::Event(_) => {}
            }
        }
    }

    /// Say goodbye so the room drops the caster at once rather than after a
    /// timeout.
    fn leave(self) {
        let Session {
            signaling,
            mut rtc,
            socket_event,
            ..
        } = self;
        rtc.disconnect();
        signaling.close();
        unsafe {
            let _ = WSACloseEvent(socket_event);
        }
    }
}

/// The local address the OS would use to reach `remote` (no packet is sent:
/// "connecting" a UDP socket only picks a route).
fn route_to(remote: SocketAddr) -> Result<IpAddr, String> {
    let probe = UdpSocket::bind("0.0.0.0:0").map_err(io)?;
    probe
        .connect(remote)
        .map_err(|_| format!("no network route to {}", remote.ip()))?;
    Ok(probe.local_addr().map_err(io)?.ip())
}

fn io(_: std::io::Error) -> String {
    "the network is not available".to_string()
}

fn rtc_error(_: str0m::RtcError) -> Closed {
    Closed::Lost("lost the connection to the room".to_string())
}

/// Spoken wording for a refused signaling request.
fn server_error(event: &str, error: &str) -> String {
    match error {
        "banned" => "this computer was banned from the room".to_string(),
        "reserved" => "the room is reserved and its host has not opened it yet".to_string(),
        "forbidden" => "the room does not allow that".to_string(),
        "not_video_room" | "kind_source_mismatch" => "the room refused the stream".to_string(),
        _ if event == "join" => format!("the room refused to let us in: {}", error),
        _ => format!("the server refused {}: {}", event, error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
    use windows::Win32::System::Threading::GetCurrentThreadId;

    #[test]
    fn settings_round_trip() {
        let s = Settings {
            server: "https://sonic.example.com".into(),
            room: "party".into(),
            name: "DJ".into(),
            device_id: "{0.0.0.00000000}.{abc}".into(),
            device_name: "Speakers (Realtek)".into(),
            channels: Channels::Mono,
            quality: Quality::High,
        };
        assert_eq!(Settings::from_text(&s.to_text()), s);
        let partial = Settings::from_text("room=x\r\nquality=nonsense\r\nbogus\r\nname=\r\n");
        assert_eq!(partial.room, "x");
        assert_eq!(partial.quality, Settings::default().quality);
        assert_eq!(partial.name, Settings::default().name, "empty name keeps default");
    }

    #[test]
    fn room_names() {
        assert!(valid_room("Party_2-b"));
        assert!(!valid_room(""));
        assert!(!valid_room("has space"));
        assert!(!valid_room(&"x".repeat(65)));
    }

    #[test]
    fn channels_and_quality() {
        assert!(Channels::Auto.stereo(false));
        assert!(!Channels::Auto.stereo(true));
        assert!(Channels::Stereo.stereo(true));
        assert!(Quality::High.bitrate(true) <= 256_000);
        for q in Quality::ALL {
            assert_eq!(Quality::parse(q.key()), Some(q));
        }
    }

    /// Streams the first active output and, if there is one, the first
    /// active input into a real SonicRoom server, then checks the SFU
    /// reports receiving the audio. Skipped unless
    /// `AUDIOMAN_SONICROOM_TEST_SERVER` names a server (e.g.
    /// `http://localhost:3100`); `AUDIOMAN_SONICROOM_TEST_MS` sets how long
    /// each device streams.
    #[test]
    fn streams_into_a_room() {
        let Ok(server) = std::env::var("AUDIOMAN_SONICROOM_TEST_SERVER") else {
            eprintln!("skipped: AUDIOMAN_SONICROOM_TEST_SERVER not set");
            return;
        };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let main_thread = unsafe { GetCurrentThreadId() };
        let ms: u64 = std::env::var("AUDIOMAN_SONICROOM_TEST_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000);
        let all = sources();
        let picks = [
            all.iter().find(|s| !s.is_input),
            all.iter().find(|s| s.is_input),
        ];
        for (source, quality) in picks.into_iter().flatten().zip([Quality::Lowest, Quality::Low]) {
            let settings = Settings {
                server: server.clone(),
                room: "audioman-test".into(),
                name: format!("audioman test {}", source.name),
                quality,
                ..Settings::default()
            };
            let c = Caster::start(&settings, source, main_thread)
                .unwrap_or_else(|e| panic!("{}: {}", source.name, e));
            let deadline = Instant::now() + Duration::from_secs(10);
            while c.phase() == Phase::Connecting && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            assert_eq!(c.phase(), Phase::Live, "{}", source.name);
            std::thread::sleep(Duration::from_millis(ms));
            assert_eq!(c.phase(), Phase::Live, "{}", source.name);
            let st = &c.stats;
            let packets = st.packets.load(Ordering::Relaxed);
            let received = st.received_seq.load(Ordering::Relaxed);
            eprintln!(
                "{} ({}): capture period {} frames, {} packets sent, SFU received up to seq {}, rtt {} ms, peak level {}/1000, ~{} ms before the network",
                source.name,
                quality.name(),
                st.capture_period.load(Ordering::Relaxed),
                packets,
                received,
                st.rtt_ms.load(Ordering::Relaxed),
                st.peak_milli.load(Ordering::Relaxed),
                st.estimated_delay_ms(quality)
            );
            let expected = ms * 1000 / (quality.frame_samples() as u64 * 1000 / 48);
            assert!(packets * 10 >= expected * 8, "only {} of ~{} packets", packets, expected);
            assert!(received > 0, "the SFU never reported receiving audio");
            drop(c);
        }
    }

    /// A participant removes the caster ("Remove caster" in SonicRoom): the
    /// stream must end as `Removed`, not as a failure to retry. Same server
    /// variable as above.
    #[test]
    fn removed_by_the_room() {
        let Ok(server) = std::env::var("AUDIOMAN_SONICROOM_TEST_SERVER") else {
            eprintln!("skipped: AUDIOMAN_SONICROOM_TEST_SERVER not set");
            return;
        };
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let Some(source) = sources().into_iter().find(|s| !s.is_input) else {
            eprintln!("skipped: no active output");
            return;
        };
        let settings = Settings {
            server: server.clone(),
            room: "audioman-kick-test".into(),
            name: "audioman kick test".into(),
            ..Settings::default()
        };
        let c = Caster::start(&settings, &source, unsafe { GetCurrentThreadId() }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while c.phase() == Phase::Connecting && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(c.phase(), Phase::Live);

        let never = || false;
        let url = socketio::websocket_url(&server).unwrap();
        let mut person = socketio::Client::connect(&url, &never).unwrap();
        let joined = person
            .request(
                "join",
                json!({ "roomName": "audioman-kick-test", "displayName": "listener" }),
                REQUEST_TIMEOUT,
                &never,
            )
            .unwrap();
        let caster_id = joined["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["displayName"] == "audioman kick test")
            .and_then(|p| p["peerId"].as_str())
            .expect("the caster is in the room")
            .to_string();
        let kicked = person
            .request(
                "kick-caster",
                json!({ "targetId": caster_id }),
                REQUEST_TIMEOUT,
                &never,
            )
            .unwrap();
        assert_eq!(kicked["ok"], true, "{}", kicked);

        let deadline = Instant::now() + Duration::from_secs(5);
        while c.phase() == Phase::Live && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(c.phase(), Phase::Removed);
        person.close();
    }
}
