//! Snappy block format, raw (no CRC framing).
//!
//! The service expects data compressed with Snappy's block format. Encoding
//! only has to be *valid*, so it emits literals only, exactly like the Python
//! client — that keeps request bodies byte-for-byte identical. Decoding uses
//! the reference implementation, which handles every copy tag the service
//! emits.

use snap::raw::Decoder;

use super::{Error, Result};

/// Largest literal run the encoder emits in one tag (Snappy's documented
/// recommendation for a 1-byte length tag is 60 bytes).
const LITERAL_CHUNK: usize = 60;

/// Compress `data`, emitting literal runs only.
pub(crate) fn compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / LITERAL_CHUNK + 10);
    let mut length = data.len();
    while length > 127 {
        out.push(((length & 0x7F) as u8) | 0x80);
        length >>= 7;
    }
    out.push(length as u8);
    for chunk in data.chunks(LITERAL_CHUNK) {
        out.push(((chunk.len() - 1) << 2) as u8);
        out.extend_from_slice(chunk);
    }
    out
}

/// Decompress a Snappy block.
pub(crate) fn decompress(buf: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = Decoder::new();
    decoder
        .decompress_vec(buf)
        .map_err(|error| Error::Protocol(format!("snappy: {error}")))
}
