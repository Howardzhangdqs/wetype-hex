//! Minimal protobuf wire-format writer and reader.
//!
//! WeType tunnels a fake HTTP request inside protobuf messages, so a
//! hand-rolled encoder is simpler than generated bindings. Only the wire types
//! the protocol uses are modelled: varint (0), 64-bit (1), length-delimited
//! (2), and 32-bit (5).

use super::{Error, Result};

/// Maximum frame size accepted by [`parse`], matching the Python client.
const MAX_SIZE: usize = 32 * 1024 * 1024;

/// Encode a `uint64` as a protobuf varint.
pub(crate) fn varint(value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    let mut value = value;
    while value > 0x7F {
        out.push(((value & 0x7F) as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

/// Accumulates protobuf fields in declaration order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Create an empty writer.
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append already-encoded bytes.
    pub(crate) fn raw(&mut self, data: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(data);
        self
    }

    /// Append a varint field.
    pub(crate) fn varint_field(&mut self, field: u32, value: u64) -> &mut Self {
        self.buf.extend_from_slice(&varint(u64::from(field) << 3));
        self.buf.extend_from_slice(&varint(value));
        self
    }

    /// Append a length-delimited (wire type 2) field.
    pub(crate) fn bytes_field(&mut self, field: u32, value: &[u8]) -> &mut Self {
        self.buf
            .extend_from_slice(&varint((u64::from(field) << 3) | 2));
        self.buf.extend_from_slice(&varint(value.len() as u64));
        self.buf.extend_from_slice(value);
        self
    }

    /// Append a string field (length-delimited UTF-8).
    pub(crate) fn str_field(&mut self, field: u32, value: &str) -> &mut Self {
        self.bytes_field(field, value.as_bytes())
    }

    /// Append a nested message field.
    pub(crate) fn msg_field(&mut self, field: u32, inner: &Writer) -> &mut Self {
        self.bytes_field(field, inner.as_bytes())
    }

    /// Append a `Header { 1: key, 2: value }` message field.
    pub(crate) fn kv_field(&mut self, field: u32, key: &str, value: &str) -> &mut Self {
        let mut entry = Writer::new();
        entry.str_field(1, key).str_field(2, value);
        self.bytes_field(field, entry.as_bytes())
    }

    /// Borrow the encoded bytes.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Take the encoded bytes.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Encode a single varint field, as `pb.v(field, value)` does in Python.
pub(crate) fn v(field: u32, value: u64) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.varint_field(field, value);
    writer.into_bytes()
}

/// Encode a single string field.
pub(crate) fn s(field: u32, value: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.str_field(field, value);
    writer.into_bytes()
}

/// Encode a single length-delimited field.
pub(crate) fn b(field: u32, value: &[u8]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.bytes_field(field, value);
    writer.into_bytes()
}

/// Encode a nested message field.
pub(crate) fn m(field: u32, inner: &[u8]) -> Vec<u8> {
    b(field, inner)
}

/// Encode a `Header` entry as a length-delimited field.
pub(crate) fn kv(field: u32, key: &str, value: &str) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.kv_field(field, key, value);
    writer.into_bytes()
}

/// One decoded protobuf value.
#[derive(Debug, Clone)]
pub(crate) enum Value {
    /// Wire type 0.
    Varint(u64),
    /// Wire types 1 and 5, with the wire type kept.
    Fixed(u8, Vec<u8>),
    /// Wire type 2.
    Bytes(Vec<u8>),
}

/// A decoded message: field number to the values seen, in wire order.
#[derive(Debug, Clone, Default)]
pub(crate) struct Fields {
    entries: Vec<(u32, Value)>,
}

impl Fields {
    /// The last value seen for `field`, if any.
    fn last(&self, field: u32) -> Option<&Value> {
        self.entries
            .iter()
            .rev()
            .find(|(number, _)| *number == field)
            .map(|(_, value)| value)
    }

    /// The last varint seen for `field`, or `None` when absent or not a varint.
    pub(crate) fn varint(&self, field: u32) -> Option<u64> {
        match self.last(field) {
            Some(Value::Varint(value)) => Some(*value),
            _ => None,
        }
    }

    /// The last length-delimited payload for `field`, or `None`.
    pub(crate) fn bytes(&self, field: u32) -> Option<&[u8]> {
        match self.last(field) {
            Some(Value::Bytes(value)) => Some(value),
            _ => None,
        }
    }

    /// The last length-delimited payload for `field` decoded as UTF-8 with
    /// replacement characters substituted for invalid sequences.
    pub(crate) fn string(&self, field: u32) -> Option<String> {
        self.bytes(field)
            .map(|raw| String::from_utf8_lossy(raw).into_owned())
    }

    /// Every field and value, in wire order.
    pub(crate) fn items(&self) -> impl Iterator<Item = (u32, &Value)> {
        self.entries.iter().map(|(field, value)| (*field, value))
    }

    /// Whether the message decoded no fields at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Decode a protobuf message, keeping every field and value.
pub(crate) fn parse(data: &[u8]) -> Result<Fields> {
    if data.len() > MAX_SIZE {
        return Err(Error::Protocol("message too large".into()));
    }
    let mut entries: Vec<(u32, Value)> = Vec::new();
    let mut pos = 0usize;

    fn read_varint(data: &[u8], pos: &mut usize) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = *data
                .get(*pos)
                .ok_or_else(|| Error::Protocol("truncated varint".into()))?;
            *pos += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift >= 70 {
                return Err(Error::Protocol("varint too long".into()));
            }
        }
    }

    while pos < data.len() {
        let tag = read_varint(data, &mut pos)?;
        let field = (tag >> 3) as u32;
        let wire = (tag & 7) as u8;
        if field == 0 {
            return Err(Error::Protocol("invalid field number".into()));
        }
        let value = match wire {
            0 => Value::Varint(read_varint(data, &mut pos)?),
            1 => {
                let end = (pos + 8).min(data.len());
                let bytes = data[pos..end].to_vec();
                pos += 8;
                Value::Fixed(wire, bytes)
            }
            2 => {
                let length = read_varint(data, &mut pos)? as usize;
                if length > data.len().saturating_sub(pos) {
                    return Err(Error::Protocol("truncated length-delimited field".into()));
                }
                let bytes = data[pos..pos + length].to_vec();
                pos += length;
                Value::Bytes(bytes)
            }
            5 => {
                let end = (pos + 4).min(data.len());
                let bytes = data[pos..end].to_vec();
                pos += 4;
                Value::Fixed(wire, bytes)
            }
            other => {
                return Err(Error::Protocol(format!("unsupported wire type {other}")));
            }
        };
        entries.push((field, value));
    }

    Ok(Fields { entries })
}
