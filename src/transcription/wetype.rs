//! The WeChat Input Method cloud backend.
//!
//! Hex's transcriptions go to `wetype.weixin.qq.com` over the reverse
//! engineered WSS protocol in [`crate::wetype`]. There is no model on disk:
//! `load` opens no connection, the first transcription does, and one that
//! fails is retried once on a fresh connection.
//!
//! The service has no live stream and no word timings. A live capture is
//! buffered by the engine and transcribed when it ends (the GGUF path does the
//! same when a model cannot stream), and a timed request gets one segment
//! spanning the clip.

use std::sync::{Arc, Mutex};
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

/// Samples per millisecond at the 16 kHz rate every backend receives.
const SAMPLES_PER_MS: usize = 16;

pub(crate) struct WetypeTranscriber {
    selection: TranscriptionSelection,
    /// The connection, shared with a live stream that may outlive one
    /// dictionary call. Kept warm between dictations.
    client: Arc<Mutex<Option<Client>>>,
}

impl WetypeTranscriber {
    pub(crate) fn load(selection: &TranscriptionSelection) -> Result<Self> {
        let client = Arc::new(Mutex::new(None));
        #[cfg(not(test))]
        warm(Arc::clone(&client));
        Ok(Self {
            selection: selection.clone(),
            client,
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
        // Transport errors are common after an idle connection; a stale
        // socket is replaced and the clip sent once more.
        match self.attempt(samples) {
            Ok(transcript) => Ok(final_text(transcript)),
            Err(first) => {
                self.clear_client();
                tracing::info!(error = %first, "reconnecting to the WeChat ASR service");
                self.attempt(samples)
                    .wrap_err_with(|| format!("WeChat ASR failed: {first}"))
                    .map(final_text)
            }
        }
    }

    fn attempt(
        &self,
        samples: &[f32],
    ) -> std::result::Result<CloudTranscript, crate::wetype::Error> {
        let mut guard = self
            .client
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if guard.is_none() {
            *guard = Some(Client::connect_with_timeout(REQUEST_TIMEOUT)?);
        }
        let result = guard
            .as_mut()
            .expect("the client was connected above")
            .transcribe_f32(samples, None);
        if result.is_err() {
            *guard = None;
        }
        result
    }

    fn clear_client(&self) {
        *self
            .client
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }

    /// Open a live utterance over this backend's connection. The engine
    /// drives it while the shortcut is held, publishing the service's
    /// cumulative text for the HUD's live text plate.
    pub(crate) fn open_stream(&self) -> Option<WetypeLive> {
        {
            let mut guard = self
                .client
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if guard.is_none() {
                match Client::connect_with_timeout(REQUEST_TIMEOUT) {
                    Ok(client) => *guard = Some(client),
                    Err(error) => {
                        tracing::warn!(%error, "could not connect the WeChat live stream");
                        return None;
                    }
                }
            }
        }
        match crate::wetype::session::VoiceStream::new(Arc::clone(&self.client)) {
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

/// Connect once in the background so the first dictation's live stream has a
/// session instead of paying the handshake at the press. Failures are silent:
/// the next use connects again.
#[cfg(not(test))]
fn warm(client: Arc<Mutex<Option<Client>>>) {
    let _ = std::thread::Builder::new()
        .name("wetype-warm".into())
        .spawn(move || {
            if let Ok(connected) = Client::connect_with_timeout(REQUEST_TIMEOUT) {
                let mut guard = client.lock().unwrap_or_else(|error| error.into_inner());
                if guard.is_none() {
                    *guard = Some(connected);
                }
            }
        });
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
