//! Live duplication of one output device onto another ("mirroring").
//!
//! Windows has no API for this, so it is built from two WASAPI halves joined
//! by a ring buffer on a dedicated real-time thread:
//!
//! * **Capture**: a shared-mode *loopback* stream on the source endpoint hands
//!   us the fully mixed signal of everything playing there.
//! * **Render**: an event-driven shared-mode stream on the target endpoint
//!   plays those frames back.
//!
//! Three details keep the delay small:
//!
//! 1. A loopback stream never signals its own event: Windows only fires
//!    loopback data in step with a *render* stream on the same endpoint. We
//!    therefore also open a **silent render stream on the source** and use its
//!    event as the capture clock. Opening it through `IAudioClient3` at the
//!    engine's minimum period has a second effect: the source engine then runs
//!    at that small period, so loopback packets shrink too.
//! 2. The target stream is also opened with `IAudioClient3` at its minimum
//!    period. Low-latency periods are only granted for the device's own mix
//!    format, so when both devices share a format (48 kHz float stereo on
//!    most machines) frames are copied verbatim. Otherwise the target stream
//!    asks the engine to convert (`AUTOCONVERTPCM`), which costs the default
//!    10 ms period but still works for any channel count / sample rate.
//! 3. The ring buffer aims to hold a small cushion ([`Latency`] decides how
//!    small). The two devices run on independent clocks, so the level drifts:
//!    when it climbs past the high-water mark we drop the excess, when it
//!    runs dry we pad silence and re-cushion. Both are glitches, but rare.
//!
//! Runtime failures (a device unplugged, the engine stalling) end the thread,
//! which posts [`WM_MIRROR_STOPPED`] so the main loop can announce it.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent,
    WaitForMultipleObjects,
};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

use crate::devices::id_string;
use crate::hidden::config_path;

/// Posted to the main thread when the engine stops on its own (device lost,
/// engine stalled). The main loop drops its [`Mirror`] and announces
/// [`Mirror::failure`].
pub const WM_MIRROR_STOPPED: u32 = WM_APP + 2;

/// How aggressively to chase low delay. Smaller cushions mean less delay but
/// more sensitivity to drivers that deliver audio in uneven bursts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Latency {
    /// Engine minimum period on both devices (often 3 ms), one period of
    /// cushion. Roughly 5 to 15 ms of added delay on good drivers.
    Lowest,
    /// Default 10 ms periods, one period of cushion. Roughly 20 to 30 ms.
    Low,
    /// Default periods with a generous cushion for drivers that stutter at
    /// anything tighter. Roughly 60 to 80 ms.
    Safe,
}

impl Latency {
    pub const ALL: [Latency; 3] = [Latency::Lowest, Latency::Low, Latency::Safe];

    /// Combo-box label.
    pub fn label(self) -> &'static str {
        match self {
            Latency::Lowest => "Lowest (smallest delay, may crackle on some drivers)",
            Latency::Low => "Low",
            Latency::Safe => "Safe (more delay, never crackles)",
        }
    }

    /// Short spoken name.
    pub fn name(self) -> &'static str {
        match self {
            Latency::Lowest => "lowest latency",
            Latency::Low => "low latency",
            Latency::Safe => "safe latency",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Latency::Lowest => "lowest",
            Latency::Low => "low",
            Latency::Safe => "safe",
        }
    }

    fn parse(s: &str) -> Option<Latency> {
        Latency::ALL.into_iter().find(|l| l.key() == s.trim())
    }

    /// Whether to request the engine's minimum period (vs. its default).
    fn wants_min_period(self) -> bool {
        self == Latency::Lowest
    }

    /// Ring cushion, in halves of (source period + target period). The
    /// output takes about one of its periods per tick and the source adds
    /// one of its periods per tick, so half a period-sum already covers the
    /// phase offset between the two clocks; the rest is jitter margin.
    fn cushion_half_periods(self) -> usize {
        match self {
            Latency::Lowest => 1,
            Latency::Low => 2,
            Latency::Safe => 6,
        }
    }
}

/// Last-used dialog choices, persisted in `%APPDATA%\audioman\mirror.txt`
/// (line 1: target endpoint id, line 2: latency key).
pub struct Prefs {
    pub target_id: Option<String>,
    pub latency: Latency,
}

impl Prefs {
    pub fn load() -> Prefs {
        let mut prefs = Prefs {
            target_id: None,
            latency: Latency::Lowest,
        };
        let Some(p) = config_path("mirror.txt") else {
            return prefs;
        };
        let Ok(text) = std::fs::read_to_string(p) else {
            return prefs;
        };
        let mut lines = text.lines();
        if let Some(id) = lines.next() {
            let id = id.trim();
            if !id.is_empty() {
                prefs.target_id = Some(id.to_string());
            }
        }
        if let Some(l) = lines.next().and_then(Latency::parse) {
            prefs.latency = l;
        }
        prefs
    }

    pub fn save(target_id: &[u16], latency: Latency) {
        let Some(p) = config_path("mirror.txt") else {
            return;
        };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!("{}\r\n{}\r\n", id_string(target_id), latency.key());
        let _ = std::fs::write(p, text);
    }
}

/// Engine facts and glitch counters, filled in by the worker.
#[derive(Default)]
pub struct Stats {
    /// Ring trimmed because the source clock ran ahead of the target's.
    pub drops: AtomicU32,
    /// Silence padded because the ring ran dry.
    pub underruns: AtomicU32,
    /// Engine period actually obtained on each side, in frames.
    pub source_period: AtomicU32,
    pub target_period: AtomicU32,
    /// Target buffer size in frames.
    pub target_buffer: AtomicU32,
    /// Ring cushion in frames.
    pub cushion: AtomicU32,
    /// Sample rate of the copied stream.
    pub rate: AtomicU32,
    /// 1 when the engine converts the format on the target side.
    pub converting: AtomicU32,
}

impl Stats {
    /// Rough added delay in milliseconds: one source period for loopback
    /// delivery, the ring cushion, and one target period in the output
    /// buffer. Device and driver latency come on top and are not visible.
    pub fn estimated_delay_ms(&self) -> u32 {
        let rate = self.rate.load(Ordering::Relaxed).max(1);
        let frames = self.source_period.load(Ordering::Relaxed)
            + self.cushion.load(Ordering::Relaxed)
            + self.target_period.load(Ordering::Relaxed);
        (frames as u64 * 1000 / rate as u64) as u32
    }
}

/// A running duplication. Dropping it stops the engine and joins its thread.
pub struct Mirror {
    stop_event: HANDLE,
    thread: Option<JoinHandle<()>>,
    failure: Arc<Mutex<Option<String>>>,
    pub stats: Arc<Stats>,
    pub source_name: String,
    pub target_name: String,
}

// HANDLE is a raw pointer newtype; the event is only ever signalled from the
// owning (main) thread and waited on by the worker, which is fine.
unsafe impl Send for Mirror {}

impl Mirror {
    /// Start duplicating `source_id` onto `target_id`. Blocks until the worker
    /// thread has opened both devices, so a failure (device busy, format
    /// rejected, ...) is reported synchronously as a spoken-friendly string.
    pub fn start(
        source_id: &[u16],
        source_name: &str,
        target_id: &[u16],
        target_name: &str,
        latency: Latency,
        main_thread: u32,
    ) -> Result<Mirror, String> {
        let stop_event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
            .map_err(|e| format!("could not create event: {}", e))?;
        let failure = Arc::new(Mutex::new(None));
        let stats = Arc::new(Stats::default());
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        let cfg = Config {
            source_id: source_id.to_vec(),
            target_id: target_id.to_vec(),
            latency,
            stats: stats.clone(),
        };
        let worker_failure = failure.clone();
        // `HANDLE` isn't Send; pass the raw value and rebuild it on the worker.
        let stop_raw = stop_event.0 as usize;
        let thread = std::thread::Builder::new()
            .name("audioman-mirror".into())
            .spawn(move || {
                let stop = HANDLE(stop_raw as *mut c_void);
                worker(cfg, stop, ready_tx, worker_failure, main_thread);
            })
            .map_err(|e| format!("could not start thread: {}", e))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Mirror {
                stop_event,
                thread: Some(thread),
                failure,
                stats,
                source_name: source_name.to_string(),
                target_name: target_name.to_string(),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                unsafe {
                    let _ = CloseHandle(stop_event);
                }
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                unsafe {
                    let _ = CloseHandle(stop_event);
                }
                Err("duplication thread died during start-up".to_string())
            }
        }
    }

    /// Why the engine stopped on its own, if it did.
    pub fn failure(&self) -> Option<String> {
        self.failure.lock().unwrap().clone()
    }
}

impl Drop for Mirror {
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
    source_id: Vec<u16>,
    target_id: Vec<u16>,
    latency: Latency,
    stats: Arc<Stats>,
}

fn worker(
    cfg: Config,
    stop: HANDLE,
    ready: mpsc::Sender<Result<(), String>>,
    failure: Arc<Mutex<Option<String>>>,
    main_thread: u32,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // Real-time scheduling for the copy loop; harmless if it fails.
    let mut task_index = 0u32;
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();

    let outcome = match unsafe { Pump::open(&cfg) } {
        Err(e) => {
            let _ = ready.send(Err(e));
            None
        }
        Ok(mut pump) => {
            let _ = ready.send(Ok(()));
            Some(unsafe { pump.run(stop) })
        }
    };

    if let Some(Err(reason)) = outcome {
        *failure.lock().unwrap() = Some(reason);
        unsafe {
            let _ = PostThreadMessageW(
                main_thread,
                WM_MIRROR_STOPPED,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            );
        }
    }

    if let Some(h) = mmcss {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
    unsafe {
        CoUninitialize();
    }
}

/// An owned copy of a `WAVEFORMATEX` (+ any `WAVEFORMATEXTENSIBLE` tail), so
/// the engine-allocated one can be freed immediately.
struct Format {
    bytes: Vec<u8>,
}

impl Format {
    unsafe fn mix_format(client: &IAudioClient) -> Result<Format, String> {
        let p = client
            .GetMixFormat()
            .map_err(|e| format!("mix format unavailable: {}", e))?;
        let header: WAVEFORMATEX = std::ptr::read_unaligned(p);
        let len = std::mem::size_of::<WAVEFORMATEX>() + header.cbSize as usize;
        let bytes = std::slice::from_raw_parts(p as *const u8, len).to_vec();
        CoTaskMemFree(Some(p as *const c_void));
        Ok(Format { bytes })
    }

    fn as_ptr(&self) -> *const WAVEFORMATEX {
        self.bytes.as_ptr() as *const WAVEFORMATEX
    }

    fn header(&self) -> WAVEFORMATEX {
        unsafe { std::ptr::read_unaligned(self.as_ptr()) }
    }

    fn frame_bytes(&self) -> usize {
        self.header().nBlockAlign.max(1) as usize
    }

    fn rate(&self) -> u32 {
        self.header().nSamplesPerSec.max(1)
    }
}

/// One opened shared-mode stream plus the period it actually got.
struct Stream {
    client: IAudioClient,
    /// Engine period in frames (best knowledge; the default period if the
    /// low-latency path was unavailable).
    period_frames: u32,
    buffer_frames: u32,
}

/// Open an event-driven shared-mode stream on `device` in `fmt`, preferring
/// `IAudioClient3` with the minimum (or default) engine period and falling
/// back to a classic `Initialize` with the engine's default buffering.
unsafe fn open_stream(
    device: &IMMDevice,
    fmt: &Format,
    extra_flags: u32,
    latency: Latency,
    event: HANDLE,
    what: &str,
) -> Result<Stream, String> {
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .map_err(|e| format!("{}: activation failed: {}", what, e))?;
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | extra_flags;
    let default_period = fmt.rate() / 100; // 10 ms
    let mut period_frames = default_period;

    let mut initialized = false;
    if let Ok(client3) = client.cast::<IAudioClient3>() {
        let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
        if client3
            .GetSharedModeEnginePeriod(fmt.as_ptr(), &mut def, &mut fund, &mut min, &mut max)
            .is_ok()
            && def > 0
        {
            let want = if latency.wants_min_period() && min > 0 {
                min
            } else {
                def
            };
            #[cfg(test)]
            eprintln!(
                "{}: engine periods default {} fundamental {} min {} max {}, asking {}",
                what, def, fund, min, max, want
            );
            let first = client3.InitializeSharedAudioStream(flags, want, fmt.as_ptr(), None);
            #[cfg(test)]
            if let Err(e) = &first {
                eprintln!(
                    "{}: InitializeSharedAudioStream({}) failed: {}",
                    what, want, e
                );
            }
            if first.is_ok() {
                period_frames = want;
                initialized = true;
            } else if want != def
                && client3
                    .InitializeSharedAudioStream(flags, def, fmt.as_ptr(), None)
                    .is_ok()
            {
                period_frames = def;
                initialized = true;
            }
        }
    }
    if !initialized {
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, fmt.as_ptr(), None)
            .map_err(|e| format!("{}: could not open stream: {}", what, e))?;
    }
    client
        .SetEventHandle(event)
        .map_err(|e| format!("{}: event registration failed: {}", what, e))?;
    let buffer_frames = client
        .GetBufferSize()
        .map_err(|e| format!("{}: buffer size unavailable: {}", what, e))?;
    Ok(Stream {
        client,
        period_frames,
        buffer_frames,
    })
}

struct Pump {
    // Silent render stream on the source: the capture clock.
    clock: Stream,
    clock_render: IAudioRenderClient,
    clock_event: HANDLE,
    // Loopback capture on the source.
    capture_client: IAudioClient,
    capture: IAudioCaptureClient,
    // Render on the target.
    out: Stream,
    out_render: IAudioRenderClient,
    out_event: HANDLE,

    frame_bytes: usize,
    ring: VecDeque<u8>,
    /// Bytes to accumulate before (re)starting playback.
    cushion: usize,
    /// Above this many bytes the ring is trimmed back to `cushion`.
    high_water: usize,
    primed: bool,
    stats: Arc<Stats>,
}

impl Pump {
    unsafe fn open(cfg: &Config) -> Result<Pump, String> {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("device enumerator unavailable: {}", e))?;
        let source = enumerator
            .GetDevice(PCWSTR(cfg.source_id.as_ptr()))
            .map_err(|_| "source device not found".to_string())?;
        let target = enumerator
            .GetDevice(PCWSTR(cfg.target_id.as_ptr()))
            .map_err(|_| "target device not found".to_string())?;

        let clock_event = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| format!("could not create event: {}", e))?;
        let out_event = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| format!("could not create event: {}", e))?;
        let close_events = |a: HANDLE, b: HANDLE| {
            let _ = CloseHandle(a);
            let _ = CloseHandle(b);
        };

        let result: Result<Pump, String> = (|| {
            // Source side: read the mix format once and use it for all three
            // streams, so frames move between them without conversion.
            let probe: IAudioClient = source
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("source device: activation failed: {}", e))?;
            let fmt = Format::mix_format(&probe)?;
            drop(probe);

            let clock = open_stream(&source, &fmt, 0, cfg.latency, clock_event, "source device")?;
            let clock_render: IAudioRenderClient = clock
                .client
                .GetService()
                .map_err(|e| format!("source device: render service failed: {}", e))?;

            let capture_client: IAudioClient = source
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("source device: activation failed: {}", e))?;
            // Generous capture buffer: we drain it on every clock tick, but a
            // late tick must not lose audio. 200 ms in 100 ns units.
            capture_client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    2_000_000,
                    0,
                    fmt.as_ptr(),
                    None,
                )
                .map_err(|e| format!("source device: loopback capture failed: {}", e))?;
            let capture: IAudioCaptureClient = capture_client
                .GetService()
                .map_err(|e| format!("source device: capture service failed: {}", e))?;

            // Target side: same bytes if the mix formats agree, otherwise let
            // the engine convert (costs the low-latency period).
            let target_probe: IAudioClient = target
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("target device: activation failed: {}", e))?;
            let target_fmt = Format::mix_format(&target_probe)?;
            drop(target_probe);
            let convert = target_fmt.bytes != fmt.bytes;
            let extra = if convert {
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
            } else {
                0
            };
            let out = match open_stream(
                &target,
                &fmt,
                extra,
                cfg.latency,
                out_event,
                "target device",
            ) {
                Ok(s) => s,
                // Some drivers reject conversion flags up front; retry asking
                // for conversion even when the formats looked identical.
                Err(_) if !convert => open_stream(
                    &target,
                    &fmt,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    cfg.latency,
                    out_event,
                    "target device",
                )?,
                Err(e) => return Err(e),
            };
            let out_render: IAudioRenderClient = out
                .client
                .GetService()
                .map_err(|e| format!("target device: render service failed: {}", e))?;

            let frame_bytes = fmt.frame_bytes();
            let period_sum = (clock.period_frames + out.period_frames) as usize * frame_bytes;
            let cushion = period_sum / 2 * cfg.latency.cushion_half_periods();
            let cushion = cushion - cushion % frame_bytes;
            let high_water = cushion + 2 * period_sum;

            let st = &cfg.stats;
            st.source_period
                .store(clock.period_frames, Ordering::Relaxed);
            st.target_period.store(out.period_frames, Ordering::Relaxed);
            st.target_buffer.store(out.buffer_frames, Ordering::Relaxed);
            st.cushion
                .store((cushion / frame_bytes) as u32, Ordering::Relaxed);
            st.rate.store(fmt.rate(), Ordering::Relaxed);
            st.converting.store(u32::from(convert), Ordering::Relaxed);

            // Pre-fill the silent clock stream, then start everything. The
            // capture must be running before the first clock tick drains it.
            fill_silence(&clock.client, &clock_render, clock.buffer_frames)?;
            capture_client
                .Start()
                .map_err(|e| format!("source device: capture start failed: {}", e))?;
            clock
                .client
                .Start()
                .map_err(|e| format!("source device: start failed: {}", e))?;
            out.client
                .Start()
                .map_err(|e| format!("target device: start failed: {}", e))?;

            Ok(Pump {
                clock,
                clock_render,
                clock_event,
                capture_client,
                capture,
                out,
                out_render,
                out_event,
                frame_bytes,
                ring: VecDeque::with_capacity(high_water * 2),
                cushion,
                high_water,
                primed: false,
                stats: cfg.stats.clone(),
            })
        })();

        if result.is_err() {
            close_events(clock_event, out_event);
        }
        result
    }

    /// Copy until `stop` is signalled (Ok) or the engine fails (Err).
    unsafe fn run(&mut self, stop: HANDLE) -> Result<(), String> {
        let handles = [stop, self.clock_event, self.out_event];
        loop {
            // Both engines tick every few ms; two seconds of silence from both
            // means something is wrong (device removed without an error, ...).
            let signalled = WaitForMultipleObjects(&handles, false, 2000);
            match signalled.0.wrapping_sub(WAIT_OBJECT_0.0) {
                0 => return Ok(()),
                1 => self.on_clock_tick().map_err(describe)?,
                2 => self.on_output_tick().map_err(describe)?,
                _ if signalled == WAIT_TIMEOUT => {
                    return Err("the audio engine stopped responding".to_string())
                }
                _ => return Err("waiting for audio events failed".to_string()),
            }
        }
    }

    /// Source engine period elapsed: top up the silent stream and move every
    /// pending loopback packet into the ring.
    unsafe fn on_clock_tick(&mut self) -> windows::core::Result<()> {
        let padding = self.clock.client.GetCurrentPadding()?;
        let avail = self.clock.buffer_frames.saturating_sub(padding);
        if avail > 0 {
            let _ = self.clock_render.GetBuffer(avail)?;
            self.clock_render
                .ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        }

        loop {
            let next = self.capture.GetNextPacketSize()?;
            if next == 0 {
                break;
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            self.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            let len = frames as usize * self.frame_bytes;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                self.ring.extend(std::iter::repeat(0u8).take(len));
            } else {
                self.ring.extend(std::slice::from_raw_parts(data, len));
            }
            self.capture.ReleaseBuffer(frames)?;
        }

        // Clock drift (source faster than target): trim back to the cushion.
        if self.ring.len() > self.high_water {
            let excess = self.ring.len() - self.cushion;
            // Keep frame alignment.
            let excess = excess - excess % self.frame_bytes;
            self.ring.drain(..excess);
            self.stats.drops.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Target engine period elapsed: hand it as many frames as it can take.
    unsafe fn on_output_tick(&mut self) -> windows::core::Result<()> {
        let padding = self.out.client.GetCurrentPadding()?;
        let avail = self.out.buffer_frames.saturating_sub(padding);
        if avail == 0 {
            return Ok(());
        }

        if !self.primed {
            if self.ring.len() >= self.cushion {
                self.primed = true;
            } else {
                let _ = self.out_render.GetBuffer(avail)?;
                return self
                    .out_render
                    .ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
            }
        }

        let have_frames = (self.ring.len() / self.frame_bytes) as u32;
        let frames = avail.min(have_frames);
        let bytes = frames as usize * self.frame_bytes;
        let dst = self.out_render.GetBuffer(avail)?;
        {
            let (a, b) = self.ring.as_slices();
            let from_a = a.len().min(bytes);
            std::ptr::copy_nonoverlapping(a.as_ptr(), dst, from_a);
            if from_a < bytes {
                std::ptr::copy_nonoverlapping(b.as_ptr(), dst.add(from_a), bytes - from_a);
            }
        }
        self.ring.drain(..bytes);
        let total = avail as usize * self.frame_bytes;
        if bytes < total {
            // Underrun (target faster than source, or a late source tick):
            // pad with silence and rebuild the cushion before resuming.
            std::ptr::write_bytes(dst.add(bytes), 0, total - bytes);
            self.primed = false;
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
        }
        self.out_render.ReleaseBuffer(avail, 0)
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        unsafe {
            let _ = self.out.client.Stop();
            let _ = self.clock.client.Stop();
            let _ = self.capture_client.Stop();
            let _ = CloseHandle(self.clock_event);
            let _ = CloseHandle(self.out_event);
        }
    }
}

unsafe fn fill_silence(
    client: &IAudioClient,
    render: &IAudioRenderClient,
    buffer_frames: u32,
) -> Result<(), String> {
    let padding = client
        .GetCurrentPadding()
        .map_err(|e| format!("source device: padding unavailable: {}", e))?;
    let avail = buffer_frames.saturating_sub(padding);
    if avail > 0 {
        render
            .GetBuffer(avail)
            .map_err(|e| format!("source device: buffer unavailable: {}", e))?;
        render
            .ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            .map_err(|e| format!("source device: buffer release failed: {}", e))?;
    }
    Ok(())
}

/// Turn a WASAPI error into something worth speaking.
fn describe(e: windows::core::Error) -> String {
    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        "a device was disconnected".to_string()
    } else {
        format!("audio error {}", e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
    use windows::Win32::System::Threading::GetCurrentThreadId;

    /// Runs the engine for a moment between the first two active outputs on
    /// this machine, at every latency profile. Skips when there is only one.
    #[test]
    fn mirrors_between_two_outputs() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let outs = crate::devices::outputs();
        if outs.len() < 2 {
            eprintln!("skipped: fewer than two active outputs");
            return;
        }
        let main_thread = unsafe { GetCurrentThreadId() };
        for latency in Latency::ALL {
            let m = Mirror::start(
                &outs[0].id,
                &outs[0].name,
                &outs[1].id,
                &outs[1].name,
                latency,
                main_thread,
            )
            .unwrap_or_else(|e| panic!("{:?}: {}", latency, e));
            let ms: u64 = std::env::var("AUDIOMAN_MIRROR_TEST_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2000);
            std::thread::sleep(Duration::from_millis(ms));
            assert_eq!(m.failure(), None, "{:?}", latency);
            let st = &m.stats;
            eprintln!(
                "{:?}: {} -> {}: periods {}/{} frames, target buffer {}, cushion {}, rate {}, converting {}, ~{} ms; drops {}, underruns {}",
                latency,
                outs[0].name,
                outs[1].name,
                st.source_period.load(Ordering::Relaxed),
                st.target_period.load(Ordering::Relaxed),
                st.target_buffer.load(Ordering::Relaxed),
                st.cushion.load(Ordering::Relaxed),
                st.rate.load(Ordering::Relaxed),
                st.converting.load(Ordering::Relaxed),
                st.estimated_delay_ms(),
                st.drops.load(Ordering::Relaxed),
                st.underruns.load(Ordering::Relaxed)
            );
            drop(m);
        }
    }
}
