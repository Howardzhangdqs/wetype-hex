//! The WeType cloud client: one WSS connection, strictly request/response.
//!
//! A [`Client`] performs the handshake lazily in [`Client::connect`] and then
//! serves as many utterances and text-polish calls as the caller needs from a
//! single connection. Every request is one binary frame out and exactly one
//! frame back; the service never pushes unsolicited frames.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tungstenite::WebSocket;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::protocol::{Message, WebSocketConfig};
use tungstenite::stream::MaybeTlsStream;

use super::{
    CMD_DH, CMD_NOTIFY, CMD_POLISH, CMD_UIN, CMD_VOICE, DEVICE_MODEL, Error, HOST, OPUS_MAGIC,
    OS_TYPE, PACKETS_PER_REQUEST, PLATFORM, Result, SCRIPT_SIMPLIFIED, SIGN_KEY, SYSTEM_VERSION,
    VERSION, crypto, opus, pb, snappy,
};

/// How long to wait for the service's reply to a request.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// Delay between end-of-utterance polls.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Maximum number of end-of-utterance polls.
const MAX_POLLS: usize = 8;
/// Frame/message size cap advertised to the WebSocket layer.
const MAX_FRAME: usize = 32 * 1024 * 1024;
/// Host package name reported to the text-polish model.
const APP_NAME: &str = "com.tencent.mm";

/// A caller-supplied callback receiving `(text, polished)` after each chunk
/// the service answers while audio is still streaming.
pub type PartialHandler<'a> = &'a mut dyn FnMut(&str, &str);

/// One finished transcription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    /// Server-generated utterance id, 32 lowercase hex characters.
    pub voice_id: String,
    /// The raw recognition text, cumulative over the utterance.
    pub text: String,
    /// The polished text from the "智能整理" pass; may be empty.
    pub polished: String,
}

impl Transcript {
    /// The text to show the user: the polished text when the service produced
    /// one, otherwise the raw recognition text.
    pub fn final_text(&self) -> &str {
        if self.polished.is_empty() {
            &self.text
        } else {
            &self.polished
        }
    }
}

/// A reply to one fake HTTP request.
#[derive(Debug)]
struct Reply {
    status: u64,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// A WeType cloud recognizer connection.
///
/// Holds the socket, the session key negotiated during the handshake, and the
/// monotonically increasing task id. It is `Send`, so a caller may move it to
/// whichever thread runs the dictation session.
pub struct Client {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    task: u64,
    uin: String,
    device: String,
    session_key: Option<Vec<u8>>,
    server_public: String,
    uin_token: String,
    timeout: Duration,
    /// Whether to set the "paragraph / de-duplicate / rewrite" switches
    /// (fields 22–24) on voice requests.
    polish: bool,
}

impl Client {
    /// Connect to the service with the default 20 s timeout.
    pub fn connect() -> Result<Client> {
        Self::connect_with_timeout(DEFAULT_TIMEOUT)
    }

    /// Connect and complete the handshake, waiting at most `timeout` for each
    /// reply from the service.
    pub fn connect_with_timeout(timeout: Duration) -> Result<Client> {
        // A `&str` request needs no `url` crate feature; the scheme survives
        // into the request URI, which is what `connect_with_config` reads to
        // decide between `ws` and `wss`.
        let mut request = format!("wss://{HOST}/").into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_static("wxws_pb"),
        );
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        let (ws, _response) = tungstenite::client::connect_with_config(request, Some(config), 3)?;

        let mut client = Client {
            ws,
            task: 0,
            uin: "0".to_owned(),
            device: String::new(),
            session_key: None,
            server_public: String::new(),
            uin_token: String::new(),
            timeout,
            polish: true,
        };
        client.set_read_timeout()?;
        if let Err(error) = client.handshake() {
            // Python closes the socket when the handshake fails; dropping the
            // connection has the same effect here, but close politely first.
            let _ = client.ws.close(None);
            return Err(error);
        }
        Ok(client)
    }

    /// Transcribe 16 kHz mono `i16` PCM.
    ///
    /// `on_partial` receives `(text, polished)` with the service's cumulative
    /// text after every streaming chunk.
    pub fn transcribe(
        &mut self,
        pcm: &[i16],
        on_partial: Option<PartialHandler<'_>>,
    ) -> Result<Transcript> {
        let mut on_partial = on_partial;
        let voice_id = token_hex(16)?;
        let mut seq: u64 = 0;
        let mut total: usize = 0;
        let mut text = String::new();
        let mut polished = String::new();

        let mut encoder = opus::OpusEncoder::new()?;
        let mut batch: Vec<Vec<u8>> = Vec::with_capacity(PACKETS_PER_REQUEST);
        for frame in frames(pcm) {
            batch.push(encoder.encode(&frame)?);
            if batch.len() >= PACKETS_PER_REQUEST {
                seq += 1;
                let chunk = pack(&batch, seq == 1);
                total += chunk.len();
                let (text_new, polished_new, _) =
                    self.voice(&voice_id, Some(&chunk), seq, total, false)?;
                if !text_new.is_empty() {
                    text = text_new;
                    if let Some(callback) = on_partial.as_mut() {
                        callback(&text, &polished_new);
                    }
                }
                if !polished_new.is_empty() {
                    polished = polished_new;
                }
                batch.clear();
            }
        }

        seq += 1;
        let chunk = pack(&batch, seq == 1);
        total += chunk.len();
        let chunk = if chunk.is_empty() { None } else { Some(chunk) };
        let (text_new, polished_new, _) =
            self.voice(&voice_id, chunk.as_deref(), seq, total, true)?;
        if !text_new.is_empty() {
            text = text_new;
        }
        if !polished_new.is_empty() {
            polished = polished_new;
        }

        for _ in 0..MAX_POLLS {
            if !polished.is_empty() {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
            let (text_new, polished_new, end_flag) = self.voice(&voice_id, None, 0, 0, true)?;
            if !text_new.is_empty() {
                text = text_new;
            }
            if !polished_new.is_empty() {
                polished = polished_new;
            }
            if end_flag != 0 && !text.is_empty() {
                break;
            }
        }

        Ok(Transcript {
            voice_id,
            text,
            polished,
        })
    }

    /// Transcribe `f32` samples in `-1.0..=1.0`, converting to `i16` first.
    pub fn transcribe_f32(
        &mut self,
        samples: &[f32],
        on_partial: Option<PartialHandler<'_>>,
    ) -> Result<Transcript> {
        if !samples.iter().all(|sample| sample.is_finite()) {
            return Err(Error::Audio("audio samples must be finite".into()));
        }
        let pcm: Vec<i16> = samples
            .iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16)
            .collect();
        self.transcribe(&pcm, on_partial)
    }

    /// Run the standalone text-polish model (文字整理) over `text`.
    ///
    /// `has_asr` marks the source as recognition output, which is the case
    /// for a dictation. Returns the rearranged text, which may equal the input
    /// when there is nothing to tidy. An empty result means the model produced
    /// no answer.
    pub fn arrange(&mut self, text: &str, has_asr: bool) -> Result<String> {
        if text.trim().is_empty() {
            return Err(Error::Protocol("text to arrange is empty".into()));
        }

        let mut req = pb::Writer::new();
        req.str_field(1, text); // source_text_list
        req.varint_field(2, SCRIPT_SIMPLIFIED); // script_type
        req.varint_field(3, u64::from(has_asr)); // has_asr
        req.varint_field(4, 0); // source_type: none
        req.str_field(5, APP_NAME); // app_name
        req.varint_field(6, 0); // app_scene: unknown
        let outer = pb::m(1, req.as_bytes());

        let body = crypto::aes_enc(self.key()?, &snappy::compress(&outer))?;
        let reply = self.roundtrip("/api_v2", &body, CMD_POLISH, "2", "")?;
        if reply.status != 200 || reply.body.is_empty() {
            return Err(Error::Server(format!(
                "polish request failed with status {}",
                reply.status
            )));
        }

        let plain = self.decode(&reply)?;
        let top = pb::parse(&plain)?;
        let fields = pb::parse(top.bytes(1).unwrap_or(&[]))?;
        let Some(raw) = fields.bytes(1) else {
            return Ok(String::new());
        };
        if raw.is_empty() {
            return Ok(String::new());
        }
        if let Ok(decoded) = std::str::from_utf8(raw)
            && !decoded.trim().is_empty()
        {
            return Ok(decoded.to_owned());
        }
        // Some builds nest TextPolishRsp one level deeper.
        Ok(pb::parse(raw)?.string(1).unwrap_or_default())
    }

    // -- lifecycle -------------------------------------------------------
    fn set_read_timeout(&mut self) -> Result<()> {
        let timeout = Some(self.timeout);
        match self.ws.get_mut() {
            MaybeTlsStream::Plain(stream) => stream.set_read_timeout(timeout)?,
            MaybeTlsStream::NativeTls(stream) => stream.get_ref().set_read_timeout(timeout)?,
            _ => {}
        }
        Ok(())
    }

    fn key(&self) -> Result<&[u8]> {
        self.session_key
            .as_deref()
            .ok_or_else(|| Error::Server("session key missing".into()))
    }

    // -- handshake -------------------------------------------------------
    fn handshake(&mut self) -> Result<()> {
        self.roundtrip("/timestamp", &[], 0, "1", "")?;

        let saved = load_identity();
        match &saved {
            Some((device, uin)) => {
                self.device = device.clone();
                self.uin = uin.clone();
                if let Err(error) = self.exchange_key(false) {
                    if !matches!(error, Error::Server(_)) {
                        return Err(error);
                    }
                    self.register()?;
                }
            }
            None => self.register()?,
        }

        let changed = match &saved {
            Some((device, uin)) => *device != self.device || *uin != self.uin,
            None => true,
        };
        if changed {
            save_identity(&self.device, &self.uin);
        }

        let notify = crypto::aes_enc(self.key()?, &[])?;
        self.roundtrip("/api_v2", &notify, CMD_NOTIFY, "1", "")?;
        Ok(())
    }

    fn exchange_key(&mut self, need_uin: bool) -> Result<()> {
        let (private, public) = crypto::keypair()?;
        let mut req = pb::Writer::new();
        req.str_field(1, &public).str_field(2, &public);
        if need_uin {
            req.varint_field(3, 1);
        }
        req.str_field(4, &self.device);
        if !need_uin {
            req.varint_field(5, self.uin.parse::<u64>().unwrap_or(0));
        }
        req.str_field(7, "")
            .str_field(8, DEVICE_MODEL)
            .str_field(9, "");

        let body = crypto::aes_enc(super::BOOT_KEY, req.as_bytes())?;
        let reply = self.request("/oauth_pubkey_v2", &body, CMD_DH, "")?;
        let fields = pb::parse(&crypto::aes_dec(super::BOOT_KEY, &reply)?)?;
        let server = fields.string(2).unwrap_or_default();
        let token = fields.string(4).unwrap_or_default();
        if server.is_empty() || (need_uin && token.is_empty()) {
            return Err(Error::Server("key exchange incomplete".into()));
        }
        self.session_key = Some(crypto::shared_x(&private, &server)?);
        self.server_public = server;
        self.uin_token = token;
        Ok(())
    }

    fn register(&mut self) -> Result<()> {
        self.uin = "0".to_owned();
        self.device = device_code(DEVICE_MODEL, &rand_lower(12)?);
        self.exchange_key(true)?;

        let token = self.uin_token.clone();
        let mut req = pb::Writer::new();
        req.str_field(1, &self.device)
            .str_field(2, &self.server_public)
            .str_field(4, &token);
        let body = crypto::aes_enc(self.key()?, req.as_bytes())?;
        let reply = self.request("/gen_uin_v2", &body, CMD_UIN, &token)?;
        let issued = pb::parse(&crypto::aes_dec(self.key()?, &reply)?)?
            .varint(2)
            .unwrap_or(0);
        if issued == 0 {
            return Err(Error::Server("server issued no UIN".into()));
        }
        self.uin = issued.to_string();
        Ok(())
    }

    // -- transport -------------------------------------------------------
    fn request(&mut self, path: &str, body: &[u8], cmd: u64, token: &str) -> Result<Vec<u8>> {
        let reply = self.roundtrip(path, body, cmd, "1", token)?;
        if reply.status != 200 || reply.body.is_empty() {
            return Err(Error::Server(format!(
                "{path} failed with status {}",
                reply.status
            )));
        }
        Ok(reply.body)
    }

    fn roundtrip(
        &mut self,
        path: &str,
        body: &[u8],
        cmd: u64,
        compress: &str,
        token: &str,
    ) -> Result<Reply> {
        let dh = cmd == CMD_DH;
        let gen_uin = cmd == CMD_UIN;
        if !(dh || gen_uin) {
            self.task += 1;
        }
        let task_id = if dh || gen_uin { cmd } else { self.task };

        let trace = rand_lower(16)?;
        let body_md5 = crypto::md5_upper(body);
        let stamp = timestamp_ms();
        let cmd_text = cmd.to_string();
        let task_text = task_id.to_string();

        let parts: Vec<&str> = if dh {
            vec![
                OS_TYPE, VERSION, PLATFORM, &stamp, &body_md5, &trace, &cmd_text,
            ]
        } else if gen_uin {
            vec![
                OS_TYPE, VERSION, PLATFORM, &stamp, &body_md5, token, &trace, &cmd_text,
            ]
        } else {
            vec![
                OS_TYPE, VERSION, PLATFORM, &cmd_text, "0", &stamp, &body_md5, &self.uin, &trace,
                &task_text,
            ]
        };
        let sign = sign_string(&parts);

        let mut headers: Vec<(String, String)> = Vec::new();
        if !token.is_empty() {
            headers.push(("Kb-GenUinToken".into(), token.to_owned()));
        }
        headers.push(("Kb-Uin".into(), self.uin.clone()));
        if dh || gen_uin {
            headers.push(("Kb-DeviceCodeRestrictionV2".into(), "1".into()));
        }
        if !dh {
            if let Some(key) = &self.session_key {
                let start = key.len().saturating_sub(4);
                headers.push(("Kb-SharedKeySuffix".into(), latin1(&key[start..])));
            }
            headers.push(("Kb-CmdId".into(), cmd_text.clone()));
            headers.push(("Kb-SubCmdId".into(), "0".into()));
        }
        for (name, value) in [
            ("Kb-OsType", OS_TYPE),
            ("Kb-Version", VERSION),
            ("Kb-SystemVersion", SYSTEM_VERSION),
            ("Kb-PackageType", "3"),
            ("Use_DebugNet", "0"),
            ("Kb-TimeStamp", stamp.as_str()),
            ("Kb-BodyMd5", body_md5.as_str()),
            ("Kb-TraceId", trace.as_str()),
            ("Kb-TaskId", task_text.as_str()),
            ("Kb-Scene", "2"),
            ("Kb-Sign", sign.as_str()),
        ] {
            headers.push((name.to_owned(), value.to_owned()));
        }
        headers.push(("Content-Length".to_owned(), body.len().to_string()));
        for (name, value) in [
            ("Kb-CompressionType", compress),
            ("Content-Type", "application/octet-stream"),
            ("HOST", HOST),
        ] {
            headers.push((name.to_owned(), value.to_owned()));
        }

        let mut http = pb::Writer::new();
        http.str_field(1, "POST")
            .str_field(3, path)
            .str_field(4, "");
        for (name, value) in &headers {
            http.kv_field(5, name, value);
        }
        http.bytes_field(6, body);

        let mut frame = pb::Writer::new();
        frame
            .varint_field(1, 0)
            .varint_field(2, 0)
            .varint_field(3, task_id)
            .msg_field(5, &http);

        self.ws.send(Message::binary(frame.into_bytes()))?;
        // The service does not push data frames, but it may ping; those are
        // answered by tungstenite and skipped here.
        let raw = loop {
            match self.ws.read()? {
                Message::Text(text) => break text.as_bytes().to_vec(),
                Message::Binary(bytes) => break bytes.to_vec(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected WebSocket message: {other:?}"
                    )));
                }
            }
        };

        let top = pb::parse(&raw)?;
        let inner = match top.bytes(4) {
            Some(bytes) if !bytes.is_empty() => pb::parse(bytes)?,
            _ => pb::Fields::default(),
        };
        let mut reply_headers: HashMap<String, String> = HashMap::new();
        for (field, value) in inner.items() {
            if field == 5 {
                continue;
            }
            let pb::Value::Bytes(bytes) = value else {
                continue;
            };
            let Ok(kv) = pb::parse(bytes) else {
                continue;
            };
            if let (Some(k), Some(v)) = (kv.string(1), kv.string(2)) {
                reply_headers.insert(k, v);
            }
        }

        Ok(Reply {
            status: inner.varint(2).unwrap_or(0),
            headers: reply_headers,
            body: inner.bytes(5).unwrap_or(&[]).to_vec(),
        })
    }

    /// Decrypt (and, when the reply says so, snappy-decompress) a reply body.
    fn decode(&self, reply: &Reply) -> Result<Vec<u8>> {
        let mut plain = crypto::aes_dec(self.key()?, &reply.body)?;
        if reply
            .headers
            .get("Kb-CompressionType")
            .is_some_and(|value| value == "2")
        {
            plain = snappy::decompress(&plain)?;
        }
        Ok(plain)
    }

    // -- recognition -----------------------------------------------------
    /// Send one voice request and return `(text, polished, end_flag)`.
    fn voice(
        &mut self,
        voice_id: &str,
        opus_chunk: Option<&[u8]>,
        seq: u64,
        total: usize,
        is_end: bool,
    ) -> Result<(String, String, u64)> {
        let outer = voice_outer(voice_id, opus_chunk, seq, total, is_end, self.polish);
        let body = crypto::aes_enc(self.key()?, &snappy::compress(&outer))?;
        let reply = self.roundtrip("/api_v2", &body, CMD_VOICE, "2", "")?;
        if reply.status != 200 || reply.body.is_empty() {
            return Err(Error::Server(format!(
                "voice request failed with status {}",
                reply.status
            )));
        }

        let plain = self.decode(&reply)?;
        let top = pb::parse(&plain)?;
        let payload = match top.bytes(1) {
            Some(bytes) if !bytes.is_empty() => bytes,
            _ => plain.as_slice(),
        };
        let fields = pb::parse(payload)?;
        if let Some(base) = fields.bytes(1)
            && !base.is_empty()
        {
            let ret = pb::parse(base)?.varint(1).unwrap_or(0);
            if ret != 0 {
                return Err(Error::Server(format!("ime business error {ret}")));
            }
        }
        Ok((
            fields.string(4).unwrap_or_default(),
            fields.string(14).unwrap_or_default(),
            fields.varint(5).unwrap_or(0),
        ))
    }
}

/// Build `Outer { 1: Inner }` for a voice request.
///
/// `opus_chunk` is the packed chunk for this request (absent for polls), `seq`
/// is 1 for the first chunk and 0 for polls, and `total` is the number of
/// audio bytes sent so far including the 16-byte stream header.
pub(crate) fn voice_outer(
    voice_id: &str,
    opus_chunk: Option<&[u8]>,
    seq: u64,
    total: usize,
    is_end: bool,
    polish: bool,
) -> Vec<u8> {
    let mut inner = pb::Writer::new();
    inner.str_field(2, voice_id);
    if let Some(chunk) = opus_chunk
        && !chunk.is_empty()
    {
        inner.bytes_field(4, chunk);
    }
    inner.varint_field(5, 5); // audio format: Opus
    if is_end {
        inner.varint_field(6, 1);
    }
    inner.varint_field(7, seq);
    if total > 0 {
        inner.varint_field(11, total as u64);
    }
    if polish {
        inner
            .varint_field(22, 1)
            .varint_field(23, 1)
            .varint_field(24, 1);
    }
    pb::m(1, inner.as_bytes())
}

/// The device code: `"MAC"` + zero padding + model + 12 random characters,
/// followed by the uppercase-hex MD5 of that string and [`SIGN_KEY`].
pub(crate) fn device_code(model: &str, randoms: &str) -> String {
    let body = format!(
        "MAC{}{model}{randoms}",
        "0".repeat(17usize.saturating_sub(model.len()))
    );
    let digest = crypto::md5_upper(format!("{body}{SIGN_KEY}").as_bytes());
    format!("{body}{digest}")
}

/// `Kb-Sign`: uppercase-hex SHA-256 over the concatenated parts plus
/// [`SIGN_KEY`].
pub(crate) fn sign_string(parts: &[&str]) -> String {
    let mut input = parts.concat();
    input.push_str(SIGN_KEY);
    crypto::sha256_upper(input.as_bytes())
}

/// Pack Opus packets into WeType's raw container, optionally with the header.
fn pack(packets: &[Vec<u8>], include_header: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if include_header {
        out.extend_from_slice(OPUS_MAGIC);
    }
    for packet in packets {
        // Opus packets are at most a few hundred bytes; the length prefix is
        // fixed at 16 bits by the container.
        out.extend_from_slice(&(packet.len() as u16).to_le_bytes());
        out.extend_from_slice(packet);
    }
    out
}

/// 200 ms of leading silence followed by 20 ms frames, the tail padded.
fn frames(pcm: &[i16]) -> impl Iterator<Item = [i16; opus::FRAME_SAMPLES]> + '_ {
    (0..opus::LEAD_FRAMES)
        .map(|_| [0i16; opus::FRAME_SAMPLES])
        .chain(pcm.chunks(opus::FRAME_SAMPLES).map(|chunk| {
            let mut frame = [0i16; opus::FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            frame
        }))
}

/// `n` random characters from `[a-z0-9]`, like Python's `secrets.choice`.
fn rand_lower(n: usize) -> Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    // 252 is the largest multiple of 36 that fits in a byte.
    const LIMIT: u8 = 252;
    let mut out = String::with_capacity(n);
    let mut buf = [0u8; 32];
    while out.len() < n {
        getrandom::fill(&mut buf)
            .map_err(|error| Error::Crypto(format!("random source failed: {error}")))?;
        for byte in buf {
            if out.len() == n {
                break;
            }
            if byte < LIMIT {
                out.push(char::from(ALPHABET[usize::from(byte % 36)]));
            }
        }
    }
    Ok(out)
}

/// `2 * n` lowercase hex characters from `n` random bytes.
fn token_hex(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes)
        .map_err(|error| Error::Crypto(format!("random source failed: {error}")))?;
    let mut out = String::with_capacity(n * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    Ok(out)
}

/// Interpret bytes as Latin-1 characters, the way `Kb-SharedKeySuffix` is
/// carried in the Python client (the key is ASCII in practice).
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

/// Milliseconds since the Unix epoch, as the protocol's timestamps are.
fn timestamp_ms() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

/// `~/…/wetype-asr/identity.json`, shared with the Python client.
fn identity_path() -> Option<PathBuf> {
    dirs::cache_dir().map(|base| base.join("wetype-asr").join("identity.json"))
}

/// The saved `(device, uin)` pair, if a usable one exists.
fn load_identity() -> Option<(String, String)> {
    let path = identity_path()?;
    let data = std::fs::read(path).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&data).ok()?;
    let device = value.get("device")?.as_str()?.to_owned();
    let uin = value.get("uin")?.as_str()?.to_owned();
    if device.is_empty() || uin.is_empty() {
        return None;
    }
    Some((device, uin))
}

/// Persist the identity; failures are ignored, as in the Python client.
fn save_identity(device: &str, uin: &str) {
    let Some(path) = identity_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = serde_json::json!({ "device": device, "uin": uin });
    let _ = std::fs::write(path, body.to_string());
}
