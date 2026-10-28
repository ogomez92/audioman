//! Shared-mode WASAPI building blocks used by the engines that move audio in
//! real time (output duplication in `mirror.rs`, room streaming in
//! `caster.rs`): an owned copy of a stream format, opening an event-driven
//! stream at the smallest period the engine grants, and error wording.

use std::ffi::c_void;

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// An owned copy of a `WAVEFORMATEX` (+ any `WAVEFORMATEXTENSIBLE` tail), so
/// the engine-allocated one can be freed immediately.
#[derive(PartialEq)]
pub struct Format {
    pub bytes: Vec<u8>,
}

impl Format {
    /// The device's shared-mode mix format.
    pub unsafe fn mix_format(client: &IAudioClient) -> Result<Format, String> {
        let p = client
            .GetMixFormat()
            .map_err(|e| format!("mix format unavailable: {}", e))?;
        let header: WAVEFORMATEX = std::ptr::read_unaligned(p);
        let len = std::mem::size_of::<WAVEFORMATEX>() + header.cbSize as usize;
        let bytes = std::slice::from_raw_parts(p as *const u8, len).to_vec();
        CoTaskMemFree(Some(p as *const c_void));
        Ok(Format { bytes })
    }

    /// Interleaved 32-bit float PCM.
    pub fn float(rate: u32, channels: u16) -> Format {
        let block = channels * 4;
        let header = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: channels,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &header as *const WAVEFORMATEX as *const u8,
                std::mem::size_of::<WAVEFORMATEX>(),
            )
        }
        .to_vec();
        Format { bytes }
    }

    pub fn as_ptr(&self) -> *const WAVEFORMATEX {
        self.bytes.as_ptr() as *const WAVEFORMATEX
    }

    pub fn header(&self) -> WAVEFORMATEX {
        unsafe { std::ptr::read_unaligned(self.as_ptr()) }
    }

    pub fn frame_bytes(&self) -> usize {
        self.header().nBlockAlign.max(1) as usize
    }

    pub fn rate(&self) -> u32 {
        self.header().nSamplesPerSec.max(1)
    }

    pub fn channels(&self) -> u16 {
        self.header().nChannels
    }

    /// Whether samples are 32-bit floats (plain or extensible tag).
    pub fn is_float(&self) -> bool {
        let h = self.header();
        if h.wBitsPerSample != 32 {
            return false;
        }
        match h.wFormatTag {
            WAVE_FORMAT_IEEE_FLOAT => true,
            WAVE_FORMAT_EXTENSIBLE
                if self.bytes.len() >= std::mem::size_of::<WAVEFORMATEXTENSIBLE>() =>
            {
                let ext: WAVEFORMATEXTENSIBLE =
                    unsafe { std::ptr::read_unaligned(self.as_ptr() as *const WAVEFORMATEXTENSIBLE) };
                let sub = ext.SubFormat;
                sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
            }
            _ => false,
        }
    }
}

/// One opened shared-mode stream plus the period it actually got.
pub struct Stream {
    pub client: IAudioClient,
    /// Engine period in frames (best knowledge; the default period if the
    /// low-latency path was unavailable).
    pub period_frames: u32,
    pub buffer_frames: u32,
}

/// Open an event-driven shared-mode stream on `device` in `fmt`, preferring
/// `IAudioClient3` with the minimum (`min_period`) or default engine period
/// and falling back to a classic `Initialize` with the engine's default
/// buffering. Works for render and capture endpoints alike.
pub unsafe fn open_stream(
    device: &IMMDevice,
    fmt: &Format,
    extra_flags: u32,
    min_period: bool,
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
            let want = if min_period && min > 0 { min } else { def };
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

/// Fill whatever room a render stream has with silence.
pub unsafe fn fill_silence(
    client: &IAudioClient,
    render: &IAudioRenderClient,
    buffer_frames: u32,
) -> windows::core::Result<()> {
    let padding = client.GetCurrentPadding()?;
    let avail = buffer_frames.saturating_sub(padding);
    if avail > 0 {
        render.GetBuffer(avail)?;
        render.ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
    }
    Ok(())
}

/// Turn a WASAPI error into something worth speaking.
pub fn describe(e: windows::core::Error) -> String {
    if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        "a device was disconnected".to_string()
    } else {
        format!("audio error {}", e)
    }
}
