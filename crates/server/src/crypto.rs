use rand::RngCore;
use rand::rngs::OsRng;
use ring::{digest, pbkdf2};
use std::num::NonZeroU32;

const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
const PBKDF2_ITERS: u32 = 100_000;
pub(crate) const PBKDF2_PREFIX: &str = "pbkdf2$";

pub fn generate_random_str(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .into_iter()
        .map(|b| TOKEN_ALPHABET[(b as usize) % TOKEN_ALPHABET.len()] as char)
        .collect()
}

/// Constant-time digest comparison: hashes both inputs with SHA-256 and folds all 32 bytes
/// without short-circuiting so byte-by-byte content differences do not leak via early exit.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    let da = digest::digest(&digest::SHA256, a.as_bytes());
    let db = digest::digest(&digest::SHA256, b.as_bytes());
    let mut diff = 0u8;
    for (&x, &y) in da.as_ref().iter().zip(db.as_ref().iter()) {
        diff |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(diff) == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Hash a panel password with random 16-byte salt and PBKDF2-HMAC-SHA256.
/// Format: `pbkdf2$<iters>$<salt_hex>$<dk_hex>`.
pub fn hash_password(plain: &str) -> String {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let mut dk = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ITERS).expect("non-zero iterations"),
        &salt,
        plain.as_bytes(),
        &mut dk,
    );
    format!(
        "{PBKDF2_PREFIX}{PBKDF2_ITERS}${}${}",
        hex_encode(&salt),
        hex_encode(&dk)
    )
}

/// Verify a candidate password against a stored PBKDF2 hash (or legacy plaintext fallback).
pub fn verify_password(candidate: &str, stored: &str) -> bool {
    if let Some(rest) = stored.strip_prefix(PBKDF2_PREFIX) {
        let mut parts = rest.split('$');
        let Some(iters_str) = parts.next() else {
            return false;
        };
        let Some(salt_hex) = parts.next() else {
            return false;
        };
        let Some(dk_hex) = parts.next() else {
            return false;
        };
        if parts.next().is_some() {
            return false;
        }
        let Ok(iters) = iters_str.parse::<u32>() else {
            return false;
        };
        let Some(iters_nz) = NonZeroU32::new(iters) else {
            return false;
        };
        let (Some(salt), Some(dk)) = (hex_decode(salt_hex), hex_decode(dk_hex)) else {
            return false;
        };
        pbkdf2::verify(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iters_nz,
            &salt,
            candidate.as_bytes(),
            &dk,
        )
        .is_ok()
    } else {
        constant_time_eq(candidate, stored)
    }
}
