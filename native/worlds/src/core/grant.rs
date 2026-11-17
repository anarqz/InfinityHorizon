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
    /// The signed host (WORLDS_OFFICIAL.md §3): a player host's decimal XUID, or
    /// `official:<vmId>` for an official world (the engine's own spelling of `{official: vmId}`;
    /// never a player identity).
    pub host_xuid: String,
    pub role: String,
    pub iat: i64,
    pub exp: i64,
    pub relay: Option<GrantRelay>,
    /// CR-L53 `sync` (MATCHMAKING_API §5f): the seat's mod-sync ids; absent = empty.
    pub sync: SyncSet,
    pub voice_blocked_until: Option<i64>,
}

/// A seat's mod-sync ids (MATCHMAKING_API §5f, OD-248): registry ids 1..=255, ascending, unique,
/// at most [`SyncSet::MAX`]. The world never reads the registry; it compares ids only. Empty
/// (the default) means no sync at all: fail closed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SyncSet {
    ids: [u8; SyncSet::MAX],
    len: u8,
}

impl SyncSet {
    pub const MAX: usize = 8;
    pub const EMPTY: SyncSet = SyncSet {
        ids: [0; SyncSet::MAX],
        len: 0,
    };

    /// The strict form: a JSON array of integers in 1..=255, strictly ascending, at most
    /// [`SyncSet::MAX`] long; `None` for anything else.
    pub fn parse(v: &Value) -> Option<SyncSet> {
        let Value::Array(items) = v else { return None };
        if items.len() > Self::MAX {
            return None;
        }
        let mut out = SyncSet::EMPTY;
        for item in items {
            let id = u8::try_from(item.as_u64()?).ok().filter(|id| *id != 0)?;
            if out.ids().last().is_some_and(|last| *last >= id) {
                return None;
            }
            out.ids[usize::from(out.len)] = id;
            out.len += 1;
        }
        Some(out)
    }

    /// The reading of the grant's `sync` and of `world.hostSync`: absent or invalid is the empty
    /// set, never an error.
    pub fn lenient(v: Option<&Value>) -> SyncSet {
        v.and_then(SyncSet::parse).unwrap_or_default()
    }

    /// A set of these ids (sorted and deduplicated; ids past [`SyncSet::MAX`] and 0 are dropped).
    pub fn of(ids: &[u8]) -> SyncSet {
        let mut sorted: Vec<u8> = ids.iter().copied().filter(|id| *id != 0).collect();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.truncate(Self::MAX);
        let mut out = SyncSet::EMPTY;
        out.ids[..sorted.len()].copy_from_slice(&sorted);
        out.len = sorted.len() as u8;
        out
    }

    pub fn ids(&self) -> &[u8] {
        &self.ids[..usize::from(self.len)]
    }

    pub fn contains(&self, id: u8) -> bool {
        self.ids().contains(&id)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The ids both sets hold.
    pub fn intersect(&self, other: &SyncSet) -> SyncSet {
        let both: Vec<u8> = self
            .ids()
            .iter()
            .copied()
            .filter(|id| other.contains(*id))
            .collect();
        SyncSet::of(&both)
    }
}

/// What the authenticated control plane says the grant must be for.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub world: &'a str,
    /// A decimal XUID, or `official:<vmId>` (as [`Claims::host_xuid`]).
    pub host_xuid: &'a str,
    pub peer_xuid: &'a str,
    pub role: &'a str,
    pub grant: Option<&'a str>,
}

/// A JSON value read with duplicate members refused and a depth limit.
struct Strict(Value);

/// The depth, and whether the value sits inside the top-level `sync` member (CR-L53: its value
/// has no depth limit, since any bad shape there reads as the empty set; duplicates are still
/// refused).
struct StrictVisitor(usize, bool);

impl StrictVisitor {
    fn too_deep(&self) -> bool {
        self.0 >= MAX_DEPTH && !self.1
    }
}

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
        if self.too_deep() {
            return Err(de::Error::custom("depth"));
        }
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(Depth(self.0 + 1, self.1))? {
            out.push(v.0);
        }
        Ok(Strict(Value::Array(out)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Strict, A::Error> {
        if self.too_deep() {
            return Err(de::Error::custom("depth"));
        }
        let mut out = Map::new();
        while let Some(k) = map.next_key::<String>()? {
            let in_sync = self.1 || self.0 == 0 && k == "sync";
            let v = map.next_value_seed(Depth(self.0 + 1, in_sync))?;
            if out.insert(k, v.0).is_some() {
                return Err(de::Error::custom("duplicate member"));
            }
        }
        Ok(Strict(Value::Object(out)))
    }
}

struct Depth(usize, bool);

impl<'de> de::DeserializeSeed<'de> for Depth {
    type Value = Strict;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Strict, D::Error> {
        d.deserialize_any(StrictVisitor(self.0, self.1))
    }
}

/// Parses JSON refusing duplicate members at any depth and nesting past the claims' depth
/// (except inside the top-level `sync`, whose bad shapes read as empty; serde_json's own
/// recursion limit, 128, still bounds it).
pub fn strict_json(bytes: &[u8]) -> Option<Value> {
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let v = de::DeserializeSeed::deserialize(Depth(0, false), &mut d).ok()?;
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

/// An official Worlds host's id (WORLDS_OFFICIAL.md §3, the `FI_WORLDS` entry): `[a-z0-9-]{1,32}`.
pub fn vm_id(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The engine's spelling of an official host, `official:<vmId>` (never a decimal XUID).
pub fn official_host(vm: &str) -> String {
    format!("official:{vm}")
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
    // exactly one of `{xuid}` (player-hosted, unchanged bytes) or `{official: vmId}`
    // (WORLDS_OFFICIAL.md §3): both, neither or anything else is refused
    let host_xuid = match (host.remove("xuid"), host.remove("official")) {
        (Some(Value::String(x)), None) if xuid(&x) => x,
        (None, Some(Value::String(vm))) if vm_id(&vm) => official_host(&vm),
        _ => return None,
    };
    if !host.is_empty() {
        return None;
    }
    let official = host_xuid.starts_with("official:");
    let role = take_str(&mut o, "role")?;
    let iat = take_int(&mut o, "iat")?;
    let exp = take_int(&mut o, "exp")?;
    let relay = match o.remove("relay") {
        None | Some(Value::Null) => None,
        Some(r) => Some(relay_of(r)?),
    };
    // CR-L53 (§5f, the same rules as `world.hostSync`): absent, null or anything but a canonical
    // id list is the empty set. Fail closed on sync, never on the join (mods never gate joins;
    // the C# `RelayGrantVerifier` reads it the same way)
    let sync = SyncSet::lenient(o.remove("sync").as_ref());
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
        // an official world's grants carry no `relay` claim (OD-189: direct, never relayed)
        && relay.as_ref().is_none_or(|r| !official && valid_relay(r))
        // the Worlds engine is an official world's host: its grants are joiner grants only
        // (WORLDS_OFFICIAL.md §3; the launcher's verifier refuses the same)
        && (!official || role == "joiner")
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
        sync,
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
