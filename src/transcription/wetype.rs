//! The WeChat Input Method cloud backend.
//!
//! Hex's transcriptions go to `wetype.weixin.qq.com` over the reverse
//! engineered WSS protocol in [`crate::wetype`]. There is no model on disk:
//! `load` opens no connection, and each use connects when it starts.
//!
//! The service has no word timings. A dictation streams: its own connection
//! carries the utterance from the moment the shortcut is pressed, the
//! service's cumulative text feeds the live plate, and the connection is
//! closed when the dictation ends. A whole-clip request opens one on demand,
//! retries once on a fresh one when it fails, and closes it before
//! returning; a timed request gets one segment spanning the clip.
//!
//! The service closes sessions that sit idle, so no connection is kept
//! between dictations: a reused one would fail the first request of the
//! next utterance and cost that dictation its live text. Connecting at the
//! press costs the handshake (a few hundred milliseconds) while the
//! microphone is already recording, which the live plate absorbs.

use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr};

use crate::transcription::engine::{Partial, StreamFeed};
use crate::transcription::local_transcriber::TimedSpan;
use crate::transcription::models::{TranscriptionModelId, TranscriptionSelection};
use crate::transcription::transcript::{TimestampLevel, Transcript, TranscriptSegment};
use crate::wetype::{Client, Transcript as CloudTranscript};

/// One request's socket read timeout. Long clips take several requests; each
/// one answers well inside this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// A live stream's connection is opened at the press, so its handshake must
/// not hold the engine's queue for long: nothing worth streaming answers in
/// a few hundred milliseconds on a working network.
const STREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Samples per millisecond at the 16 kHz rate every backend receives.
const SAMPLES_PER_MS: usize = 16;

pub(crate) struct WetypeTranscriber {
    selection: TranscriptionSelection,
    /// This whole-clip request's connection, opened on demand and closed when
    /// the request ends. Nothing is kept between uses: the service closes
    /// idle sessions, so a later request would fail on the dead socket.
    client: Option<Client>,
}

impl WetypeTranscriber {
    pub(crate) fn load(selection: &TranscriptionSelection) -> Result<Self> {
        Ok(Self {
            selection: selection.clone(),
            client: None,
        })
    }

    /// The cloud has no streaming API Hex can drive between feeds, so the
    /// engine buffers the clip and calls [`Self::transcribe_clip`].
    pub(crate) fn stream_options(&self) -> Option<&transcribe_cpp::StreamOptions> {
        None
    }

    pub(crate) fn stream_trailing_silence(&self) -> usize {
        0
    }

    pub(crate) fn model_id(&self) -> TranscriptionModelId {
        self.selection.model
    }

    /// Never reached: [`Self::stream_options`] is always `None`, so the
    /// engine never opens a stream on this backend.
    pub(crate) fn with_stream<R>(
        &mut self,
        _stream_options: &transcribe_cpp::StreamOptions,
        _cancel: &transcribe_cpp::CancelToken,
        _drive: impl FnOnce(&mut transcribe_cpp::Stream<'_>) -> R,
    ) -> transcribe_cpp::Result<R> {
        unreachable!("the WeChat cloud backend has no live stream")
    }

    pub(crate) fn gguf_selection(&self) -> Option<&TranscriptionSelection> {
        Some(&self.selection)
    }

    pub(crate) fn matches_selection(&self, selection: &TranscriptionSelection) -> bool {
        &self.selection == selection
    }

    /// Nothing to warm: a connection opens with the first transcription.
    pub(crate) fn prewarm(&mut self) -> Result<()> {
        Ok(())
    }

    pub(crate) fn transcribe_clip(&mut self, samples: Vec<f32>) -> Result<String> {
        self.transcribe(&samples)
    }

    /// The cloud gives no native timings, so a timed request gets the whole
    /// clip as one segment.
    pub(crate) fn transcribe_timed(&mut self, samples: Vec<f32>) -> Result<Transcript> {
        let text = self.transcribe(&samples)?;
        let duration_ms = (samples.len() / SAMPLES_PER_MS) as i64;
        Ok(transcript_with_span(text, duration_ms))
    }

    /// Segment-level timings at best: one span for the clip.
    pub(crate) fn timestamps(&self) -> TimestampLevel {
        TimestampLevel::Segment
    }

    pub fn device_label(&self) -> &str {
        "WeChat cloud"
    }

    pub fn transcribe_words(&mut self, samples: &[f32]) -> Result<(String, Vec<TimedSpan>)> {
        let text = self.transcribe(samples)?;
        let duration_ms = (samples.len() / SAMPLES_PER_MS) as i64;
        let spans = if text.is_empty() {
            Vec::new()
        } else {
            vec![(0, duration_ms, text.clone())]
        };
        Ok((text, spans))
    }

    pub(crate) fn transcribe_segments(&mut self, samples: Vec<f32>) -> Result<Transcript> {
        self.transcribe_timed(samples)
    }

    pub fn transcribe(&mut self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        // A transport error sends the clip once more on a fresh connection;
        // either way the connection is closed when this request ends.
        let result = match self.attempt(samples) {
            Ok(transcript) => Ok(final_text(transcript)),
            Err(first) => {
                tracing::info!(error = %first, "reconnecting to the WeChat ASR service");
                self.attempt(samples)
                    .wrap_err_with(|| format!("WeChat ASR failed: {first}"))
                    .map(final_text)
            }
        };
        self.client = None;
        result
    }

    fn attempt(
        &mut self,
        samples: &[f32],
    ) -> std::result::Result<CloudTranscript, crate::wetype::Error> {
        if self.client.is_none() {
            self.client = Some(Client::connect_with_timeout(REQUEST_TIMEOUT)?);
        }
        let result = self
            .client
            .as_mut()
            .expect("the client was connected above")
            .transcribe_f32(samples, None);
        if result.is_err() {
            self.client = None;
        }
        result
    }

    /// Open a live utterance on a connection of its own. The engine drives it
    /// while the shortcut is held, publishing the service's cumulative text
    /// for the HUD's live text plate; the stream closes the connection when
    /// it ends, so nothing sits idle between dictations.
    ///
    /// A connection left in `client` (by a whole-clip request) is deliberately
    /// not reused: the service closes idle sessions, and the first request on
    /// a closed one is what would cost this dictation its live text.
    pub(crate) fn open_stream(&self) -> Option<WetypeLive> {
        let client = match Client::connect_with_timeout(STREAM_CONNECT_TIMEOUT) {
            Ok(client) => client,
            Err(error) => {
                tracing::warn!(%error, "could not connect the WeChat live stream");
                return None;
            }
        };
        match crate::wetype::session::VoiceStream::new(client) {
            Ok(inner) => Some(WetypeLive { inner }),
            Err(error) => {
                // The connection itself is still usable for a whole-clip
                // request; only this utterance's encoder failed.
                tracing::warn!(%error, "could not start the WeChat live stream");
                None
            }
        }
    }
}

fn final_text(transcript: CloudTranscript) -> String {
    transcript.final_text().trim().to_owned()
}

/// The engine's view of a live cloud utterance.
pub(crate) struct WetypeLive {
    inner: crate::wetype::session::VoiceStream,
}

impl StreamFeed for WetypeLive {
    fn feed(&mut self, samples: &[f32]) -> std::result::Result<(), String> {
        self.inner.feed(samples).map_err(|error| error.to_string())
    }

    fn finalize(&mut self) -> std::result::Result<String, String> {
        self.inner.finish().map_err(|error| error.to_string())
    }

    fn partial(&self) -> Partial {
        let (stable, tentative) = self.inner.partial();
        Partial {
            stable: stable.to_owned(),
            tentative: tentative.to_owned(),
        }
    }
}

fn transcript_with_span(text: String, duration_ms: i64) -> Transcript {
    let segments = (!text.is_empty())
        .then(|| TranscriptSegment {
            start_ms: 0,
            end_ms: duration_ms,
            text: text.clone(),
        })
        .into_iter()
        .collect();
    Transcript {
        text,
        segments,
        words: Vec::new(),
    }
}
