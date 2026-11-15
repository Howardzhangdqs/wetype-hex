//! Cryptographic primitives used by the WeType wire protocol.
//!
//! * secp128r1 ECDH, implemented on [`BigUint`] because no mainstream library
//!   ships this curve;
//! * AES-256-ECB with PKCS#7 padding;
//! * MD5 and SHA-256 digests in the uppercase-hex form the protocol signs with.

use std::sync::OnceLock;

use aes::Aes256;
use aes::cipher::{Block, BlockDecrypt, BlockEncrypt, KeyInit};
// `md-5` and `sha2` reach different `digest` majors, so each trait is imported
// under its own name; the types never implement both.
use md5::Digest as Md5Digest;
use md5::Md5;
use num_bigint::BigUint;
use num_traits::{One, Zero};
use sha2::Digest as Sha256Digest;
use sha2::Sha256;

use super::{Error, Result};

/// An affine curve point, or `None` for the point at infinity.
pub(crate) type Point = Option<(BigUint, BigUint)>;

/// The secp128r1 domain parameters.
struct Curve {
    p: BigUint,
    a: BigUint,
    n: BigUint,
    g: (BigUint, BigUint),
}

fn parse(hex: &str) -> BigUint {
    BigUint::parse_bytes(hex.as_bytes(), 16).expect("static curve constant is hex")
}

fn curve() -> &'static Curve {
    static CURVE: OnceLock<Curve> = OnceLock::new();
    CURVE.get_or_init(|| Curve {
        p: parse("FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFF"),
        a: parse("FFFFFFFDFFFFFFFFFFFFFFFFFFFFFFFC"),
        n: parse("FFFFFFFE0000000075A30D1B9038A115"),
        g: (
            parse("161FF7528B899B2D0C28607CA52C5B86"),
            parse("CF5AC8395BAFEB13C02DA292DDED7A83"),
        ),
    })
}

/// The curve generator.
pub(crate) fn generator() -> Point {
    Some(curve().g.clone())
}

/// `x mod p` inverse for a prime `p`, via Fermat's little theorem.
fn inverse(x: &BigUint) -> BigUint {
    let curve = curve();
    x.modpow(&(curve.p.clone() - BigUint::from(2u32)), &curve.p)
}

/// `(x - y) mod p`, without letting a [`BigUint`] subtraction underflow.
fn sub_mod(x: &BigUint, y: &BigUint) -> BigUint {
    let p = &curve().p;
    if x >= y { (x - y) % p } else { (x + p - y) % p }
}

/// Add two curve points (the usual chord-and-tangent rules).
pub(crate) fn add(p1: &Point, p2: &Point) -> Point {
    let (x1, y1) = match p1 {
        Some(point) => point,
        None => return p2.clone(),
    };
    let (x2, y2) = match p2 {
        Some(point) => point,
        None => return p1.clone(),
    };
    let curve = curve();
    let p = &curve.p;
    // A point and its negation.
    if x1 == x2 && (y1 + y2) % p == BigUint::zero() {
        return None;
    }
    let slope = if p1 == p2 {
        let numerator = BigUint::from(3u32) * x1 * x1 + &curve.a;
        numerator * inverse(&((BigUint::from(2u32) * y1) % p)) % p
    } else {
        sub_mod(y2, y1) * inverse(&sub_mod(x2, x1)) % p
    };
    let x3 = (&slope * &slope - x1 - x2) % p;
    let y3 = sub_mod(&(&slope * sub_mod(x1, &x3)), y1);
    Some((x3, y3))
}

/// Scalar multiplication by double-and-add.
pub(crate) fn mul(k: &BigUint, point: &Point) -> Point {
    let mut result: Point = None;
    let mut addend = point.clone();
    let mut k = k.clone();
    while k > BigUint::zero() {
        if k.bit(0) {
            result = add(&result, &addend);
        }
        addend = add(&addend, &addend);
        k >>= 1;
    }
    result
}

/// A coordinate as 32 uppercase hex digits.
fn hex128(value: &BigUint) -> String {
    let digits = value.to_str_radix(16).to_uppercase();
    format!("{digits:0>32}")
}

/// Encode an affine point as `"04" || X || Y`, 66 uppercase hex characters.
pub(crate) fn encode_public(x: &BigUint, y: &BigUint) -> String {
    format!("04{}{}", hex128(x), hex128(y))
}

/// A uniformly random scalar in `1..n`, the same range as the Python client.
fn random_scalar() -> Result<BigUint> {
    let limit = curve().n.clone() - BigUint::one();
    loop {
        let mut buf = [0u8; 16];
        getrandom::fill(&mut buf)
            .map_err(|error| Error::Crypto(format!("random source failed: {error}")))?;
        let candidate = BigUint::from_bytes_be(&buf);
        if candidate < limit {
            return Ok(candidate + BigUint::one());
        }
    }
}

/// Generate an ephemeral key pair: `(private scalar, "04"||X||Y hex)`.
pub(crate) fn keypair() -> Result<(BigUint, String)> {
    loop {
        let private = random_scalar()?;
        if let Some((x, y)) = mul(&private, &generator()) {
            return Ok((private, encode_public(&x, &y)));
        }
    }
}

/// The ECDH shared X coordinate as 32 uppercase-hex ASCII bytes, which is used
/// verbatim as the AES-256 session key.
pub(crate) fn shared_x(private: &BigUint, peer_hex: &str) -> Result<Vec<u8>> {
    let peer = peer_hex.as_bytes();
    if peer.len() != 66 || !peer.starts_with(b"04") {
        return Err(Error::Crypto("invalid server public key".into()));
    }
    let coordinate = |slice: &[u8]| {
        BigUint::parse_bytes(slice, 16)
            .ok_or_else(|| Error::Crypto("invalid server public key".into()))
    };
    let x = coordinate(&peer[2..34])?;
    let y = coordinate(&peer[34..66])?;
    let point = mul(private, &Some((x, y)))
        .ok_or_else(|| Error::Crypto("degenerate shared point".into()))?;
    Ok(hex128(&point.0).into_bytes())
}

/// PKCS#7 pad to the AES block size (always appends at least one byte).
fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 16;
    let padding = BLOCK - data.len() % BLOCK;
    let mut out = Vec::with_capacity(data.len() + padding);
    out.extend_from_slice(data);
    out.resize(data.len() + padding, padding as u8);
    out
}

/// Remove PKCS#7 padding, rejecting malformed blocks like Python's `unpad`.
fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>> {
    let padding = *data
        .last()
        .ok_or_else(|| Error::Crypto("invalid padding".into()))? as usize;
    if padding == 0 || padding > 16 || padding > data.len() {
        return Err(Error::Crypto("invalid padding".into()));
    }
    if !data[data.len() - padding..]
        .iter()
        .all(|byte| usize::from(*byte) == padding)
    {
        return Err(Error::Crypto("invalid padding".into()));
    }
    Ok(data[..data.len() - padding].to_vec())
}

fn cipher(key: &[u8]) -> Result<Aes256> {
    Aes256::new_from_slice(key)
        .map_err(|_| Error::Crypto(format!("invalid AES-256 key length {}", key.len())))
}

/// AES-256-ECB encrypt with PKCS#7 padding.
pub(crate) fn aes_enc(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let cipher = cipher(key)?;
    let mut out = pkcs7_pad(data);
    for block in out.chunks_exact_mut(16) {
        cipher.encrypt_block(Block::<Aes256>::from_mut_slice(block));
    }
    Ok(out)
}

/// AES-256-ECB decrypt, removing PKCS#7 padding.
pub(crate) fn aes_dec(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return Err(Error::Crypto("ciphertext is not a whole block".into()));
    }
    let cipher = cipher(key)?;
    let mut out = data.to_vec();
    for block in out.chunks_exact_mut(16) {
        cipher.decrypt_block(Block::<Aes256>::from_mut_slice(block));
    }
    pkcs7_unpad(&out)
}

/// Uppercase-hex MD5 of `data`.
pub(crate) fn md5_upper(data: &[u8]) -> String {
    format!("{:X}", Md5::digest(data))
}

/// Uppercase-hex SHA-256 of `data`.
pub(crate) fn sha256_upper(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}
