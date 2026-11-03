// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Relay-grant verification (contract §6.2, MATCHMAKING_API §4): Ed25519 over
//! `"fi-relay-grant-v1\n" ‖ claims` with the ring key `kid` names, on the exact received bytes
//! (never re-serialized), then strict claims (no unknown or duplicate member, the C#
//! `RelayGrantVerifier`'s checks) and the hosting context `(world, host, xuid, role, grant)`.
//!
//! The signature check is `verify_strict` (it also refuses small-order keys and non-canonical
//! signatures, as the platform's own relay does); the ring refuses keys that do not decompress
//! or are of small order. Failures carry no claims, signature or identity.

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use super::keys::{KeyRing, decode_base64, valid_kid};

pub const MAX_CLAIMS_BYTES: usize = 1024;
pub const MAX_LIFETIME_SECS: i64 = 60;
pub const FUTURE_SKEW_SECS: i64 = 30;
const SIGN_PREFIX: &[u8] = b"fi-relay-grant-v1\n";
/// JSON nesting the claims may use (the reference's `MaxDepth`).
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Invalid,
    Expired,
    ContextMismatch,
}

/// The signed relay both sides meet at (CR-S57).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRelay {
    pub key: String,
    pub addr: String,
    pub binding: u16,
    pub port: u16,
}

/// The verified claims the engine uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub grant: String,
    pub world: String,
    pub xuid: String,
    pub host_xuid: String,
    pub role: String,
    pub iat: i64,
    pub exp: i64,
    pub relay: Option<GrantRelay>,
    pub voice_blocked_until: Option<i64>,
}

/// What the authenticated control plane says the grant must be for.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub world: &'a str,
    pub host_xuid: &'a str,
    pub peer_xuid: &'a str,
    pub role: &'a str,
    pub grant: Option<&'a str>,
}

/// A JSON value read with duplicate members refused and a depth limit.
struct Strict(Value);

struct StrictVisitor(usize);

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Strict;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Strict, E> {
        Ok(Strict(Value::Bool(v)))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Strict, E> {
        Ok(Strict(Value::from(v)))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Strict, E> {
        Ok(Strict(Value::from(v)))
    }
    fn visit_f64<E>(self, v: f64) -> Result<Strict, E> {
        Ok(Strict(Value::from(v)))
    }
    fn visit_str<E>(self, v: &str) -> Result<Strict, E> {
        Ok(Strict(Value::String(v.to_string())))
    }
    fn visit_string<E>(self, v: String) -> Result<Strict, E> {
        Ok(Strict(Value::String(v)))
    }
    fn visit_unit<E>(self) -> Result<Strict, E> {
        Ok(Strict(Value::Null))
    }
    fn visit_none<E>(self) -> Result<Strict, E> {
        Ok(Strict(Value::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Strict, A::Error> {
        if self.0 >= MAX_DEPTH {
            return Err(de::Error::custom("depth"));
        }
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(Depth(self.0 + 1))? {
            out.push(v.0);
        }
        Ok(Strict(Value::Array(out)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
        if self.0 >= MAX_DEPTH {
            return Err(de::Error::custom("depth"));
        }
        let mut out = Map::new();
        while let Some(k) = map.next_key::<String>()? {
            let v = map.next_value_seed(Depth(self.0 + 1))?;
            if out.insert(k, v.0).is_some() {
                return Err(de::Error::custom("duplicate member"));
            }
        }
        Ok(Strict(Value::Object(out)))
    }
}

struct Depth(usize);

impl<'de> de::DeserializeSeed<'de> for Depth {
    type Value = Strict;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Strict, D::Error> {
        d.deserialize_any(StrictVisitor(self.0))
    }
}

/// Parses JSON refusing duplicate members and nesting past the claims' depth.
pub fn strict_json(bytes: &[u8]) -> Option<Value> {
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let v = de::DeserializeSeed::deserialize(Depth(0), &mut d).ok()?;
    d.end().ok()?;
    Some(v.0)
}

/// A canonical lowercase non-nil UUID (`"D"` format).
pub fn uuid(s: &str) -> bool {
    uuid_bytes(s).is_some_and(|b| b != [0u8; 16])
}

/// The 16 bytes of a canonical lowercase UUID, in text order (the reference's big-endian form).
pub fn uuid_bytes(s: &str) -> Option<[u8; 16]> {
    let b = s.as_bytes();
    if b.len() != 36 {
        return None;
    }
    let mut out = [0u8; 16];
    let mut n = 0;
    let mut i = 0;
    while i < 36 {
        if matches!(i, 8 | 13 | 18 | 23) {
            if b[i] != b'-' {
                return None;
            }
            i += 1;
            continue;
        }
        let hex = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        out[n] = (hex(b[i])? << 4) | hex(b[i + 1])?;
        n += 1;
        i += 2;
    }
    Some(out)
}

/// A canonical decimal XUID other than 0 (no sign, spaces, separators or leading zero).
pub fn xuid(s: &str) -> bool {
    s.parse::<u64>().is_ok_and(|x| x != 0 && x.to_string() == s)
}

fn take_str(o: &mut Map<String, Value>, k: &str) -> Option<String> {
    match o.remove(k)? {
        Value::String(s) => Some(s),
        _ => None,
    }
}

fn take_int(o: &mut Map<String, Value>, k: &str) -> Option<i64> {
    o.remove(k)?.as_i64()
}

fn relay_of(v: Value) -> Option<GrantRelay> {
    let Value::Object(mut o) = v else { return None };
    let key = take_str(&mut o, "key")?;
    let addr = take_str(&mut o, "addr")?;
    let binding = u16::try_from(o.remove("binding")?.as_u64()?).ok()?;
    let port = u16::try_from(o.remove("port")?.as_u64()?).ok()?;
    if !o.is_empty() {
        return None;
    }
    Some(GrantRelay {
        key,
        addr,
        binding,
        port,
    })
}

/// CR-S57 `relay`: a 32-byte key in canonical base64, an IPv4 literal or a DNS name, two
/// non-zero ports.
fn valid_relay(r: &GrantRelay) -> bool {
    if decode_base64(&r.key).is_none_or(|k| k.len() != 32) || r.binding == 0 || r.port == 0 {
        return false;
    }
    let a = &r.addr;
    if !(1..=253).contains(&a.len()) || a.starts_with('.') || a.ends_with('.') || a.contains("..") {
        return false;
    }
    if a.bytes().all(|c| c.is_ascii_digit() || c == b'.') && a.matches('.').count() == 3 {
        return a.parse::<std::net::Ipv4Addr>().is_ok();
    }
    a.bytes()
        .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
        && a.split('.')
            .all(|l| (1..=63).contains(&l.len()) && !l.starts_with('-') && !l.ends_with('-'))
}

/// The strict claims; `None` when a member is unknown, missing, null where it may not be, or of
/// the wrong type.
fn claims_of(v: Value) -> Option<Claims> {
    let Value::Object(mut o) = v else { return None };
    let version = o.remove("v")?.as_u64()?;
    let _kid = take_str(&mut o, "kid")?;
    let grant = take_str(&mut o, "grant")?;
    let world = take_str(&mut o, "world")?;
    let sub = take_str(&mut o, "sub")?;
    let xuid_s = take_str(&mut o, "xuid")?;
    let tier = take_str(&mut o, "tier")?;
    let Value::Object(mut host) = o.remove("host")? else {
        return None;
    };
    let host_xuid = take_str(&mut host, "xuid")?;
    if !host.is_empty() {
        return None;
    }
    let role = take_str(&mut o, "role")?;
    let iat = take_int(&mut o, "iat")?;
    let exp = take_int(&mut o, "exp")?;
    let relay = match o.remove("relay") {
        None | Some(Value::Null) => None,
        Some(r) => Some(relay_of(r)?),
    };
    let voice_blocked_until = match o.remove("voiceBlockedUntil") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_i64()?),
    };
    if !o.is_empty() || version > u64::from(u32::MAX) {
        return None;
    }
    let ok = version == 1
        && matches!(role.as_str(), "host" | "joiner")
        && matches!(tier.as_str(), "free" | "vip")
        && exp > iat
        && i128::from(exp) - i128::from(iat) <= i128::from(MAX_LIFETIME_SECS)
        && uuid(&grant)
        && uuid(&world)
        && uuid(&sub)
        && xuid(&xuid_s)
        && xuid(&host_xuid)
        && relay.as_ref().is_none_or(valid_relay)
        && voice_blocked_until.is_none_or(|v| v >= 0);
    ok.then_some(Claims {
        grant,
        world,
        xuid: xuid_s,
        host_xuid,
        role,
        iat,
        exp,
        relay,
        voice_blocked_until,
    })
}

/// The base64 of the wire (`claims`, `signature`): canonical standard base64 of at most `max`
/// bytes.
pub fn decode_field(text: &str, max: usize) -> Option<Vec<u8>> {
    if text.is_empty() || text.len() > max.div_ceil(3) * 4 {
        return None;
    }
    decode_base64(text).filter(|b| b.len() <= max)
}

/// Verifies a grant's raw claims and signature at `now` (Unix seconds) for `context`.
pub fn verify_raw(
    claims: &[u8],
    signature: &[u8],
    ring: &KeyRing,
    now: i64,
    context: Context,
) -> Result<Claims, Failure> {
    if claims.is_empty() || claims.len() > MAX_CLAIMS_BYTES || now < 0 {
        return Err(Failure::Invalid);
    }
    let signature: &[u8; 64] = signature.try_into().map_err(|_| Failure::Invalid)?;
    // only the key choice reads the unverified bytes
    let unverified = strict_json(claims).ok_or(Failure::Invalid)?;
    let kid = unverified
        .get("kid")
        .and_then(Value::as_str)
        .filter(|k| valid_kid(k))
        .ok_or(Failure::Invalid)?;
    let key = ring.verifying_key(kid).ok_or(Failure::Invalid)?;
    let mut message = Vec::with_capacity(SIGN_PREFIX.len() + claims.len());
    message.extend_from_slice(SIGN_PREFIX);
    message.extend_from_slice(claims);
    key.verify_strict(&message, &ed25519_dalek::Signature::from_bytes(signature))
        .map_err(|_| Failure::Invalid)?;
    let c = claims_of(unverified).ok_or(Failure::Invalid)?;
    if i128::from(c.iat) - i128::from(now) > i128::from(FUTURE_SKEW_SECS) {
        return Err(Failure::Invalid);
    }
    if c.exp <= now {
        return Err(Failure::Expired);
    }
    if c.world != context.world
        || c.host_xuid != context.host_xuid
        || c.xuid != context.peer_xuid
        || c.role != context.role
        || context.grant.is_some_and(|g| g != c.grant)
    {
        return Err(Failure::ContextMismatch);
    }
    Ok(c)
}

/// [`verify_raw`] on the wire's base64 fields.
pub fn verify(
    claims_b64: &str,
    signature_b64: &str,
    ring: &KeyRing,
    now: i64,
    context: Context,
) -> Result<(Claims, Vec<u8>, Vec<u8>), Failure> {
    let claims = decode_field(claims_b64, MAX_CLAIMS_BYTES).ok_or(Failure::Invalid)?;
    let signature = decode_field(signature_b64, 64).ok_or(Failure::Invalid)?;
    let c = verify_raw(&claims, &signature, ring, now, context)?;
    Ok((c, claims, signature))
}
