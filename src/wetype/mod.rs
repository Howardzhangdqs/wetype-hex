//! WeType cloud speech recognition.
//!
//! A faithful port of the reverse-engineered WeType wire protocol: a
//! `wss://wetype.weixin.qq.com/` connection speaking the `wxws_pb`
//! subprotocol, where every frame is a protobuf envelope carrying a fake HTTP
//! request whose body is AES-256-ECB over Snappy over more protobuf.
//!
//! ```text
//! Envelope { 1:0, 2:0, 3:task_id, 5:HttpRequest { 1:"POST", 3:path, 4:"",
//!            5:[Header], 6:body } }
//! Envelope { 1:0, 2:1, 4:Reply { 2:status, 3:"OK", 4:[Header], 5:body } }
//! ```
//!
//! Handshake: `/timestamp` → `/oauth_pubkey_v2` (secp128r1 ECDH) →
//! `/gen_uin_v2` (anonymous UIN) → `/api_v2` notify. Handshake bodies are
//! AES-256-ECB/PKCS7 encrypted; the session key is the uppercase-hex ECDH
//! shared X coordinate used directly as 32 ASCII bytes.
//!
//! Voice: `/api_v2`, cmd 4548, body `AES(snappy(Outer{1: Inner}))`; the audio
//! is `#!OPUS_RAW_V1` `02` `01` `00` followed by repeated
//! uint16-little-endian-length-prefixed Opus packets (20 ms, 16 kHz mono).
//!
//! Nothing here has to be reached before the caller decides to use the cloud
//! recognizer, so unused items are allowed until the integration lands.
#![allow(dead_code)]

use std::fmt;

pub(crate) mod crypto;
pub(crate) mod opus;
pub(crate) mod pb;
pub(crate) mod session;
pub(crate) mod snappy;

// The two public types of the module; a caller may use either.
#[allow(unused_imports)]
pub use session::{Client, Transcript};

/// Server host, also sent as the `HOST` header.
pub(crate) const HOST: &str = "wetype.weixin.qq.com";
/// Key appended to every `Kb-Sign` input.
pub(crate) const SIGN_KEY: &str = "zN7rB3bL4pO8jW1o";
/// Bootstrap AES-256 key that protects the key-exchange request and reply.
pub(crate) const BOOT_KEY: &[u8] = b"D4Y5U3Y2M0C0T7N4P1P7O2N6E1I2Y1U6";
/// Client version advertised to the service.
pub(crate) const VERSION: &str = "2.2.3(657)";
/// `Kb-OsType`; `5` is macOS, which the service accepts from Linux too.
pub(crate) const OS_TYPE: &str = "5";
/// `Kb-Platform`.
pub(crate) const PLATFORM: &str = "2";
/// Device model used in the device code and the key exchange.
pub(crate) const DEVICE_MODEL: &str = "Mac16,12";
/// `Kb-SystemVersion`.
pub(crate) const SYSTEM_VERSION: &str = "27.0.0";

/// Command id for `/oauth_pubkey_v2` (the Diffie-Hellman exchange).
pub(crate) const CMD_DH: u64 = 2_147_483_646;
/// Command id for `/gen_uin_v2` (anonymous UIN registration).
pub(crate) const CMD_UIN: u64 = 0x7FFF_FDFD;
/// Command id for the post-handshake notify frame.
pub(crate) const CMD_NOTIFY: u64 = 8074;
/// Command id for speech recognition.
pub(crate) const CMD_VOICE: u64 = 4548;
/// Command id for the standalone text-polish model.
pub(crate) const CMD_POLISH: u64 = 4549;

/// `TextPolishReq.ScriptType` for simplified Chinese.
pub(crate) const SCRIPT_SIMPLIFIED: u64 = 1;
/// `TextPolishReq.ScriptType` for traditional Chinese.
pub(crate) const SCRIPT_TRADITIONAL: u64 = 2;

/// Opus packets sent per voice request (50 × 20 ms = 1 s).
///
/// The shipped clients send 120 ms in real time because they stream while the
/// user speaks. Hex transcribes a finished clip, so larger batches trade the
/// service's streaming granularity for far fewer round trips: 1 s keeps the
/// polish model's paragraph/list structure intact (2 s batching flattened
/// lists in testing) while running about 5x faster than real time.
pub(crate) const PACKETS_PER_REQUEST: usize = 50;
/// Header of WeType's own raw Opus stream container.
pub(crate) const OPUS_MAGIC: &[u8] = b"#!OPUS_RAW_V1\x02\x01\x00";

/// Result alias for every fallible operation in this module.
pub type Result<T> = std::result::Result<T, Error>;

/// Everything that can go wrong talking to the WeType service.
#[derive(Debug)]
pub enum Error {
    /// A socket read, write, or filesystem operation failed.
    Io(std::io::Error),
    /// The WebSocket layer failed (handshake, framing, closed connection).
    WebSocket(tungstenite::Error),
    /// A protobuf or container frame did not decode.
    Protocol(String),
    /// The service answered with an error status, business code, or an
    /// incomplete handshake.
    Server(String),
    /// Audio could not be encoded, or the caller supplied none.
    Audio(String),
    /// A cryptographic primitive failed (key, padding, public key).
    Crypto(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::WebSocket(error) => write!(f, "websocket error: {error}"),
            Self::Protocol(message) => write!(f, "protocol error: {message}"),
            Self::Server(message) => write!(f, "server error: {message}"),
            Self::Audio(message) => write!(f, "audio error: {message}"),
            Self::Crypto(message) => write!(f, "crypto error: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::WebSocket(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<tungstenite::Error> for Error {
    fn from(error: tungstenite::Error) -> Self {
        Self::WebSocket(error)
    }
}

#[cfg(test)]
mod tests;
