//! Opus encoding at the parameters the service expects.
//!
//! 16 kHz mono, 20 ms frames (320 samples), 64 kbps, complexity 9 — the same
//! settings the shipped WeType clients use. Each packet is length-prefixed
//! inside WeType's own raw container (see [`super::session`]).

use opus::{Application, Bitrate, Channels, Encoder};

use super::{Error, Result};

/// Samples per 20 ms frame at 16 kHz.
pub(crate) const FRAME_SAMPLES: usize = 320;
/// Silence prepended to every utterance, in 20 ms frames (200 ms), so the
/// service's voice-activity detector hears an onset.
pub(crate) const LEAD_FRAMES: usize = 10;
/// Target bitrate in bits per second.
const BITRATE: i32 = 64_000;
/// Encoder complexity, 0–10.
const COMPLEXITY: u8 = 9;
/// Output buffer size; Opus never emits more for one frame at this bitrate.
const MAX_PACKET: usize = 4000;

/// A 16 kHz mono Opus encoder.
pub(crate) struct OpusEncoder {
    encoder: Encoder,
}

impl OpusEncoder {
    /// Create an encoder configured like the desktop client's.
    pub(crate) fn new() -> Result<Self> {
        let mut encoder = Encoder::new(16_000, Channels::Mono, Application::Audio)
            .map_err(|error| Error::Audio(format!("opus encoder create failed: {error}")))?;
        encoder
            .set_bitrate(Bitrate::Bits(BITRATE))
            .map_err(|error| Error::Audio(format!("opus bitrate rejected: {error}")))?;
        encoder
            .set_complexity(i32::from(COMPLEXITY))
            .map_err(|error| Error::Audio(format!("opus complexity rejected: {error}")))?;
        Ok(Self { encoder })
    }

    /// Encode exactly one 20 ms frame.
    pub(crate) fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(Error::Audio(format!(
                "opus frame must be {FRAME_SAMPLES} samples, got {}",
                pcm.len()
            )));
        }
        let mut out = [0u8; MAX_PACKET];
        let written = self
            .encoder
            .encode(pcm, &mut out)
            .map_err(|error| Error::Audio(format!("opus encode failed: {error}")))?;
        if written == 0 {
            return Err(Error::Audio("opus encode produced no data".into()));
        }
        Ok(out[..written].to_vec())
    }
}
