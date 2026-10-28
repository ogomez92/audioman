//! A safe wrapper around the one part of libopus we use: a 48 kHz float
//! encoder producing one packet per frame.

use unsafe_libopus::{
    opus_encode_float, opus_encoder_create, opus_encoder_ctl, opus_encoder_destroy, OpusEncoder,
    OPUS_APPLICATION_AUDIO, OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_OK,
    OPUS_SET_BITRATE_REQUEST, OPUS_SET_INBAND_FEC_REQUEST, OPUS_SET_PACKET_LOSS_PERC_REQUEST,
    OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_MUSIC,
};

pub const SAMPLE_RATE: u32 = 48_000;

/// Largest packet we let the encoder write (way above any sane bitrate for
/// a 20 ms frame).
pub const MAX_PACKET: usize = 1500;

pub struct Encoder {
    st: *mut OpusEncoder,
    channels: usize,
}

// The encoder state is plain memory owned by this value.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `low_delay` picks libopus's restricted-low-delay mode (CELT only,
    /// 2.5 ms look-ahead instead of 6.5 ms); otherwise the general audio mode,
    /// which can switch to SILK/hybrid for better quality at low bitrates.
    pub fn new(channels: usize, low_delay: bool, bitrate: u32) -> Result<Encoder, String> {
        let application = if low_delay {
            OPUS_APPLICATION_RESTRICTED_LOWDELAY
        } else {
            OPUS_APPLICATION_AUDIO
        };
        let mut err = 0i32;
        let st = unsafe {
            opus_encoder_create(SAMPLE_RATE as i32, channels as i32, application, &mut err)
        };
        if st.is_null() || err != OPUS_OK {
            return Err(format!("could not create the Opus encoder ({})", err));
        }
        let enc = Encoder { st, channels };
        unsafe {
            opus_encoder_ctl!(st, OPUS_SET_BITRATE_REQUEST, bitrate as i32);
            opus_encoder_ctl!(st, OPUS_SET_SIGNAL_REQUEST, OPUS_SIGNAL_MUSIC);
            // In-band FEC lets listeners rebuild a lost packet from the next
            // one (the room negotiates useinbandfec=1); it only kicks in once
            // the encoder expects some loss.
            opus_encoder_ctl!(st, OPUS_SET_INBAND_FEC_REQUEST, 1);
            opus_encoder_ctl!(st, OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5);
        }
        Ok(enc)
    }

    /// Encode one frame of interleaved samples (a valid Opus frame size per
    /// channel: 120, 240, 480 or 960 at 48 kHz) into `out`.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, String> {
        let frame = (pcm.len() / self.channels) as i32;
        let n = unsafe {
            opus_encode_float(self.st, pcm.as_ptr(), frame, out.as_mut_ptr(), out.len() as i32)
        };
        usize::try_from(n).map_err(|_| format!("Opus encoding failed ({})", n))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.st) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_every_frame_size() {
        for channels in [1, 2] {
            for low_delay in [true, false] {
                let mut enc = Encoder::new(channels, low_delay, 128_000).unwrap();
                for frame in [120, 240, 480, 960] {
                    let pcm: Vec<f32> = (0..frame * channels)
                        .map(|i| (i as f32 * 0.05).sin() * 0.5)
                        .collect();
                    let mut out = [0u8; MAX_PACKET];
                    let n = enc.encode(&pcm, &mut out).unwrap();
                    assert!(n > 1, "{} ch, {} frames: {} bytes", channels, frame, n);
                }
            }
        }
    }
}
