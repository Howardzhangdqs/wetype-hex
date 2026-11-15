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

use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr};

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
        // Transport errors are common after an idle connection; a stale
        // socket is replaced and the clip sent once more.
        match self.attempt(samples) {
            Ok(transcript) => Ok(final_text(transcript)),
            Err(first) => {
                self.client = None;
                tracing::info!(error = %first, "reconnecting to the WeChat ASR service");
                self.attempt(samples)
                    .wrap_err_with(|| format!("WeChat ASR failed: {first}"))
                    .map(final_text)
            }
        }
    }

    fn attempt(
        &mut self,
        samples: &[f32],
    ) -> std::result::Result<CloudTranscript, crate::wetype::Error> {
        if self.client.is_none() {
            self.client = Some(Client::connect_with_timeout(REQUEST_TIMEOUT)?);
        }
        self.client
            .as_mut()
            .expect("the client was connected above")
            .transcribe_f32(samples, None)
    }
}

fn final_text(transcript: CloudTranscript) -> String {
    transcript.final_text().trim().to_owned()
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
