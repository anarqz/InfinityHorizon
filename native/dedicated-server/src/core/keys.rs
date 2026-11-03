// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The relay-grant public key ring (`--grant-keys`, `keys.set`): the `RelayGrantVerifyKeys` text
//! form `kid:base64,...` (`M/RelayGrantVerifier.cs:79-100`): at most 1024 characters and 8 keys,
//! kid 1-32 of `a-z0-9-`, standard canonical base64 of 32 bytes, no duplicate kid, at least one.
//! Each key must be a valid Ed25519 public key (it decompresses and is not of small order, as the
//! C# `ValidatePublicKeyFull` asks); grant verification (grant.rs) uses the parsed key.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyRing {
    keys: Vec<(String, [u8; 32])>,
}

impl KeyRing {
    pub fn parse(text: &str) -> Result<KeyRing, &'static str> {
        if text.len() > 1024 {
            return Err("too long");
        }
        let mut keys: Vec<(String, [u8; 32])> = Vec::new();
        for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let Some((kid, encoded)) = item.split_once(':') else {
                return Err("not kid:base64");
            };
            let (kid, encoded) = (kid.trim(), encoded.trim());
            if !valid_kid(kid) {
                return Err("invalid kid");
            }
            let key = decode_base64(encoded).ok_or("not standard base64")?;
            let key: [u8; 32] = key.try_into().map_err(|_| "not 32 bytes")?;
            if !full_key(&key) {
                return Err("not an Ed25519 public key");
            }
            if keys.iter().any(|(k, _)| k == kid) {
                return Err("duplicate kid");
            }
            keys.push((kid.to_string(), key));
            if keys.len() > 8 {
                return Err("more than 8 keys");
            }
        }
        if keys.is_empty() {
            return Err("no key");
        }
        Ok(KeyRing { keys })
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn key(&self, kid: &str) -> Option<&[u8; 32]> {
        self.keys.iter().find(|(k, _)| k == kid).map(|(_, v)| v)
    }

    /// The Ed25519 key `kid` names (checked when the ring was parsed).
    pub fn verifying_key(&self, kid: &str) -> Option<ed25519_dalek::VerifyingKey> {
        self.key(kid)
            .and_then(|k| ed25519_dalek::VerifyingKey::from_bytes(k).ok())
    }
}

pub fn valid_kid(kid: &str) -> bool {
    (1..=32).contains(&kid.len())
        && kid
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, canonical only (re-encoding gives the same text).
pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.is_empty() || !b.len().is_multiple_of(4) {
        return None;
    }
    let chunks = b.len() / 4;
    let mut out = Vec::with_capacity(chunks * 3);
    for (ci, chunk) in b.chunks(4).enumerate() {
        let pad = chunk.iter().rev().take_while(|c| **c == b'=').count();
        if pad > 2 || (pad > 0 && ci != chunks - 1) {
            return None;
        }
        let mut n: u32 = 0;
        for (i, c) in chunk.iter().enumerate() {
            let v = if i >= 4 - pad {
                0
            } else {
                ALPHABET.iter().position(|a| a == c)? as u32
            };
            n = (n << 6) | v;
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    (encode_base64(&out) == s).then_some(out)
}

pub fn encode_base64(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | (*chunk.get(2).unwrap_or(&0) as u32);
        for i in 0..4 {
            if i <= chunk.len() {
                s.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// The full public-key check the reference makes (BouncyCastle `ValidatePublicKeyFull`, as
/// `verify_strict` needs): a canonical encoding of a curve point, not of small order, in the
/// prime-order subgroup (torsion-free).
pub fn full_key(key: &[u8; 32]) -> bool {
    let compressed = curve25519_dalek::edwards::CompressedEdwardsY(*key);
    let Some(point) = compressed.decompress() else {
        return false;
    };
    !point.is_small_order() && point.is_torsion_free() && point.compress().to_bytes() == *key
}
