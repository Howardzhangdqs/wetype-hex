//! Known-answer tests for the WeType primitives.
//!
//! Every expected value comes from `test_vectors.json`, which was produced by
//! the Python reference client, so these tests pin the port to its wire
//! behaviour rather than merely to self-consistency.

use num_bigint::BigUint;
use serde_json::Value;

use super::{Client, SIGN_KEY, crypto, opus, pb, session, snappy};

fn vectors() -> Value {
    serde_json::from_str(include_str!("test_vectors.json")).expect("test vectors parse")
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "hex string has an odd length");
    (0..hex.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&hex[index..index + 2], 16).expect("test vector is hexadecimal")
        })
        .collect()
}

fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("test vector {key} is a string"))
}

#[test]
fn digests_and_signatures_match_the_vectors() {
    let vectors = vectors();

    let md5 = &vectors["md5"];
    assert_eq!(
        crypto::md5_upper(text(md5, "input_utf8").as_bytes()),
        text(md5, "digest_upper")
    );

    let sign = &vectors["sign"];
    let parts: Vec<&str> = sign["parts"]
        .as_array()
        .expect("sign parts are an array")
        .iter()
        .map(|part| part.as_str().expect("sign part is a string"))
        .collect();
    assert_eq!(format!("{}{SIGN_KEY}", parts.concat()), text(sign, "input"));
    assert_eq!(session::sign_string(&parts), text(sign, "digest_upper"));
}

#[test]
fn aes_256_ecb_pkcs7_matches_the_vectors() {
    let vectors = vectors();
    for index in 0..4 {
        let case = vectors
            .get(format!("aes{index}"))
            .expect("aes vector is present");
        let key = hex_bytes(text(case, "key_hex"));
        let plain = hex_bytes(text(case, "plain_hex"));
        let cipher = hex_bytes(text(case, "cipher_hex"));
        assert_eq!(
            crypto::aes_enc(&key, &plain).expect("encrypts"),
            cipher,
            "aes{index} encryption"
        );
        assert_eq!(
            crypto::aes_dec(&key, &cipher).expect("decrypts"),
            plain,
            "aes{index} decryption"
        );
    }
}

#[test]
fn secp128r1_matches_the_vectors() {
    let vectors = vectors();
    let ecdh = &vectors["ecdh"];
    let private = BigUint::from(ecdh["priv"].as_u64().expect("private scalar"));
    let peer_private = BigUint::from(ecdh["peer_priv"].as_u64().expect("peer scalar"));
    let public = text(ecdh, "pub");

    let point = crypto::mul(&private, &crypto::generator()).expect("public key is not degenerate");
    assert_eq!(
        crypto::encode_public(&point.0, &point.1),
        public,
        "public key for the vector's private scalar"
    );

    let shared = crypto::shared_x(&private, text(ecdh, "peer_pub")).expect("shared point");
    assert_eq!(
        String::from_utf8(shared.clone()).expect("shared X is ASCII"),
        text(ecdh, "shared")
    );
    assert_eq!(
        crypto::shared_x(&peer_private, public).expect("shared point"),
        shared,
        "the shared secret is symmetric"
    );

    for (multiple, key) in [(1u32, "g1"), (2, "g2"), (3, "g3")] {
        let point = crypto::mul(&BigUint::from(multiple), &crypto::generator())
            .expect("multiple of the generator is not degenerate");
        assert_eq!(
            crypto::encode_public(&point.0, &point.1),
            text(&vectors, key),
            "{key}"
        );
    }
}

#[test]
fn device_codes_match_the_vectors() {
    let vectors = vectors();
    let device = &vectors["device"];
    let model = text(device, "model");
    let randoms = text(device, "rand");

    let body = format!(
        "MAC{}{model}{randoms}",
        "0".repeat(17usize.saturating_sub(model.len()))
    );
    assert_eq!(body, text(device, "body"));
    assert_eq!(session::device_code(model, randoms), text(device, "device"));
}

#[test]
fn snappy_matches_the_vectors() {
    let vectors = vectors();

    for index in 0..4 {
        let case = vectors
            .get(format!("snappy_literal{index}"))
            .expect("snappy literal vector is present");
        let plain = hex_bytes(text(case, "plain_hex"));
        let compressed = hex_bytes(text(case, "compressed_hex"));
        assert_eq!(
            snappy::compress(&plain),
            compressed,
            "snappy_literal{index} compression"
        );
        assert_eq!(
            snappy::decompress(&compressed).expect("decompresses"),
            plain,
            "snappy_literal{index} decompression"
        );
    }

    for key in ["snappy_copy1", "snappy_copy2"] {
        let case = &vectors[key];
        let plain = hex_bytes(text(case, "plain_hex"));
        let compressed = hex_bytes(text(case, "compressed_hex"));
        assert_eq!(
            snappy::decompress(&compressed).expect("decompresses"),
            plain,
            "{key} decompression"
        );
    }
}

#[test]
fn protobuf_writers_match_the_vectors() {
    let vectors = vectors();

    let mut http = pb::Writer::new();
    http.str_field(1, "POST")
        .str_field(3, "/timestamp")
        .str_field(4, "");
    http.kv_field(5, "Kb-Uin", "123");
    http.bytes_field(6, &[0x08u8, 0x01]);
    let mut envelope = pb::Writer::new();
    envelope
        .varint_field(1, 0)
        .varint_field(2, 0)
        .varint_field(3, 7)
        .msg_field(5, &http);
    assert_eq!(
        hex_string(envelope.as_bytes()),
        text(&vectors["pb_envelope"], "hex"),
        "request envelope"
    );

    let outer = session::voice_outer(
        "0123456789abcdef0123456789abcdef",
        Some(b"OPUSDATA"),
        3,
        42,
        true,
        true,
    );
    assert_eq!(
        hex_string(&outer),
        text(&vectors["voice_outer"], "hex"),
        "voice request envelope"
    );
}

#[test]
fn opus_encodes_a_silent_frame() {
    let mut encoder = opus::OpusEncoder::new().expect("libopus is available");
    let packet = encoder
        .encode(&[0i16; opus::FRAME_SAMPLES])
        .expect("silence encodes");
    assert!(
        !packet.is_empty() && packet.len() <= 4000,
        "a 20 ms frame is 1..=4000 bytes, got {}",
        packet.len()
    );
}

#[test]
fn the_client_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Client>();
}

/// Round-trip real audio through the live service.
///
/// Set `WETYPE_TEST_RAW` to a raw little-endian s16, 16 kHz, mono PCM file and
/// run with `--ignored`; it never runs as part of the default suite.
#[test]
#[ignore = "needs network access and WETYPE_TEST_RAW pointing at s16le 16 kHz mono PCM"]
fn live_transcribe() {
    let Ok(path) = std::env::var("WETYPE_TEST_RAW") else {
        return;
    };
    let bytes = std::fs::read(&path).expect("raw PCM file reads");
    let pcm: Vec<i16> = bytes
        .chunks_exact(2)
        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
        .collect();

    let mut client = Client::connect().expect("connects to WeType");
    let transcript = client.transcribe(&pcm, None).expect("transcribes");
    assert!(
        !transcript.final_text().is_empty(),
        "the service returned an empty transcript"
    );
}
