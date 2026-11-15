//! Automatic 文字整理 for dictation.
//!
//! The WeChat service has a second model, next to speech recognition, that
//! turns a spoken paragraph into structured written text (lists, headings,
//! grouping) and leaves already-tidy short text alone. Hex's transcription
//! path only asks it on demand, through [`crate::wetype::Client::arrange`].
//! This module keeps one connection for the dictation worker so each dictation
//! can be tidied before it is copied or pasted, with no handshake per clip.

use std::sync::Mutex;
use std::time::Duration;

use super::{Client, Error};

/// A tidy request answers well inside this; a stuck service must not hold a
/// dictation hostage.
const TIMEOUT: Duration = Duration::from_secs(8);

/// One connection shared by the dictation worker. A failed request drops it,
/// so the next dictation reconnects instead of reusing a dead socket.
static CLIENT: Mutex<Option<Client>> = Mutex::new(None);

/// Tidy `text` with the service's 文字整理 model, marked as recognition
/// output. Returns the arranged text, which may equal the input; `Ok(None)`
/// when the service answered with nothing to use.
pub fn tidy(text: &str) -> Result<Option<String>, Error> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let mut guard = CLIENT.lock().unwrap_or_else(|error| error.into_inner());
    if guard.is_none() {
        *guard = Some(Client::connect_with_timeout(TIMEOUT)?);
    }
    let attempt = |client: &mut Client| client.arrange(text, true);
    let arranged = match attempt(guard.as_mut().expect("the client was connected above")) {
        Ok(arranged) => arranged,
        Err(first) => {
            *guard = None;
            tracing::info!(error = %first, "reconnecting to the WeChat text-tidy service");
            let mut client = Client::connect_with_timeout(TIMEOUT)?;
            let arranged = client.arrange(text, true)?;
            *guard = Some(client);
            arranged
        }
    };
    Ok((!arranged.trim().is_empty()).then_some(arranged))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_not_sent_to_the_service() {
        assert!(tidy("").unwrap().is_none());
        assert!(tidy("   \n").unwrap().is_none());
    }
}
