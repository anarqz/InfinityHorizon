// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! FIUP v1 (UDP_SOCKET_PROOF.md, contract §6.1): a joiner's game or hook socket proves it holds
//! its grant's `udpProof` key, and the host binds that exact endpoint to the grant (kind 0 game,
//! 1 hook) for 45 s, renewed by the next proof. Fixed frames: hello (104 bytes), challenge,
//! response and ack (147), each HMAC-SHA256 over a per-type domain and the frame. The host keeps
//! bounded state only; the keys never enter a frame, a log or an event. Ported from the C#
//! `UdpProofCodec`/`UdpProofHost` (client f81d09c).

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
};

use hmac::{KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = hmac::Hmac<Sha256>;

pub const HELLO_LEN: usize = 104;
pub const FRAME_LEN: usize = 147;
const DOMAINS: [&[u8]; 5] = [
    b"",
    b"fi-udp-proof-v1/hello\n",
    b"fi-udp-proof-v1/challenge\n",
    b"fi-udp-proof-v1/response\n",
    b"fi-udp-proof-v1/ack\n",
];
/// A binding lives this long after its proof (the host lease's length).
const BINDING_MS: u64 = 45_000;
const PENDING_MS: u64 = 5_000;
const SEEN_MS: u64 = 90_000;
/// Retransmitted hellos (or responses) answered per pending challenge (or completed proof),
/// counted per endpoint: a replay from elsewhere never spends another endpoint's retries.
const RETRIES: u32 = 16;
/// Table caps: hello rate entries, seen nonces and per-grant rates; pending challenges,
/// completed responses and bindings; distinct source IPs counted per pre-rate entry.
const TABLE_MAX: usize = 4096;
const HANDSHAKES_MAX: usize = 64;
const PRE_IPS_MAX: usize = 16;

/// The FIUP authority of one grant (from a verified punch): the frame header it must carry, its
/// key and its expiry (Unix seconds). Its fingerprint (SHA-256 of header and key, what bindings
/// and pending state remember instead of the key) is computed once, when the snapshot is built.
#[derive(Clone)]
pub struct Authority {
    pub header: [u8; 56],
    pub key: [u8; 32],
    pub expires: i64,
    fingerprint: [u8; 32],
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authority { redacted }")
    }
}

#[cfg(feature = "test-hooks")]
thread_local! {
    static FINGERPRINTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Test support: how many authority fingerprints (SHA-256) this thread has computed.
#[cfg(feature = "test-hooks")]
pub fn fingerprint_count() -> u64 {
    FINGERPRINTS.with(std::cell::Cell::get)
}

impl Authority {
    pub fn new(header: [u8; 56], key: [u8; 32], expires: i64) -> Authority {
        #[cfg(feature = "test-hooks")]
        FINGERPRINTS.with(|c| c.set(c.get() + 1));
        let mut h = Sha256::new();
        h.update(header);
        h.update(key);
        Authority {
            header,
            key,
            expires,
            fingerprint: h.finalize().into(),
        }
    }

    /// Overwrites the key (a replaced or revoked grant); nothing matches its fingerprint after.
    pub fn zero(&mut self) {
        for b in self.key.iter_mut().chain(self.fingerprint.iter_mut()) {
            // volatile-free best effort: the value is dropped right after
            *b = 0;
        }
        std::hint::black_box(&self.key);
        std::hint::black_box(&self.fingerprint);
    }

    fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }
}

/// The frame header of a grant: magic, version, grant, world, host and peer XUIDs.
pub fn header(grant: [u8; 16], world: [u8; 16], host: u64, peer: u64) -> [u8; 56] {
    let mut raw = [0u8; 56];
    raw[..4].copy_from_slice(b"FIUP");
    raw[4] = 1;
    raw[8..24].copy_from_slice(&grant);
    raw[24..40].copy_from_slice(&world);
    raw[40..48].copy_from_slice(&host.to_be_bytes());
    raw[48..56].copy_from_slice(&peer.to_be_bytes());
    raw
}

/// The FIUP header's host field for an official world (WORLDS_OFFICIAL.md §3): the official
/// host tag `0x8000000000000000 | first 8 bytes, big endian, of SHA-256("fi-official-host-v1\n" ‖
/// vmId)`: nonzero, top bit set, so outside every real XUID. It lives only in the proof header,
/// never in a claim, a row or a log line.
pub fn official_host_tag(vm_id: &str) -> u64 {
    let mut h = Sha256::new();
    h.update(b"fi-official-host-v1\n");
    h.update(vm_id.as_bytes());
    let digest = h.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(first) | 0x8000_0000_0000_0000
}

/// A unicast address (IPv4-mapped unmapped first; IPv4 first octet 1-223; IPv6 not `::` and
/// not multicast).
pub fn unicast(ip: IpAddr) -> bool {
    match canonical_ip(ip) {
        IpAddr::V4(v) => (1..224).contains(&v.octets()[0]),
        IpAddr::V6(v) => !v.is_unspecified() && !v.is_multicast(),
    }
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v => v,
    }
}

#[derive(Debug, Clone)]
struct Packet {
    ty: u8,
    kind: u8,
    grant: [u8; 16],
    peer: u64,
    raw: Vec<u8>,
    observed: Option<SocketAddr>,
    expires: u64,
}

fn parse(d: &[u8]) -> Option<Packet> {
    if !matches!(d.len(), HELLO_LEN | FRAME_LEN)
        || &d[..4] != b"FIUP"
        || d[4] != 1
        || !(1..=4).contains(&d[5])
        || d[6] > 1
        || d[7] != 0
        || (d[5] == 1) != (d.len() == HELLO_LEN)
    {
        return None;
    }
    let grant: [u8; 16] = d[8..24].try_into().ok()?;
    let world: [u8; 16] = d[24..40].try_into().ok()?;
    let host = u64::from_be_bytes(d[40..48].try_into().ok()?);
    let peer = u64::from_be_bytes(d[48..56].try_into().ok()?);
    if grant == [0; 16] || world == [0; 16] || host == 0 || peer == 0 || d[56..72] == [0; 16] {
        return None;
    }
    let (mut observed, mut expires) = (None, 0);
    if d[5] != 1 {
        if d[72..88] == [0; 16] || !matches!(d[88], 4 | 6) {
            return None;
        }
        if d[88] == 4 && d[93..105].iter().any(|b| *b != 0) {
            return None;
        }
        let ip = if d[88] == 4 {
            IpAddr::from(<[u8; 4]>::try_from(&d[89..93]).ok()?)
        } else {
            IpAddr::from(<[u8; 16]>::try_from(&d[89..105]).ok()?)
        };
        if !unicast(ip) {
            return None;
        }
        let port = u16::from_be_bytes([d[105], d[106]]);
        if port == 0 {
            return None;
        }
        observed = Some(crate::stun::plain_endpoint(SocketAddr::new(ip, port)));
        expires = u64::from_be_bytes(d[107..115].try_into().ok()?);
        if expires == 0 || expires > i64::MAX as u64 {
            return None;
        }
    }
    Some(Packet {
        ty: d[5],
        kind: d[6],
        grant,
        peer,
        raw: d.to_vec(),
        observed,
        expires,
    })
}

fn mac(frame: &[u8], key: &[u8; 32]) -> HmacSha256 {
    let payload = frame.len() - 32;
    let mut m = <HmacSha256 as KeyInit>::new_from_slice(key).expect("any key length");
    m.update(DOMAINS[usize::from(frame[5])]);
    m.update(&frame[..payload]);
    m
}

fn sign(frame: &mut [u8], key: &[u8; 32]) {
    let tag = mac(frame, key).finalize().into_bytes();
    let n = frame.len();
    frame[n - 32..].copy_from_slice(&tag);
}

fn verify(p: &Packet, key: &[u8; 32]) -> bool {
    let n = p.raw.len();
    mac(&p.raw, key).verify_slice(&p.raw[n - 32..]).is_ok()
}

fn same_context(p: &Packet, header: &[u8; 56]) -> bool {
    p.raw[8..56] == header[8..56]
}

fn challenge(hello: &Packet, endpoint: SocketAddr, expiry: u64, key: &[u8; 32]) -> Vec<u8> {
    let endpoint = crate::stun::plain_endpoint(endpoint);
    let mut raw = vec![0u8; FRAME_LEN];
    raw[..72].copy_from_slice(&hello.raw[..72]);
    raw[5] = 2;
    if getrandom::fill(&mut raw[72..88]).is_err() {
        // never a predictable nonce: an unfillable one is a random-looking hash of the frame
        let h = Sha256::digest(&hello.raw);
        raw[72..88].copy_from_slice(&h[..16]);
    }
    match endpoint.ip() {
        IpAddr::V4(v) => {
            raw[88] = 4;
            raw[89..93].copy_from_slice(&v.octets());
        }
        IpAddr::V6(v) => {
            raw[88] = 6;
            raw[89..105].copy_from_slice(&v.octets());
        }
    }
    raw[105..107].copy_from_slice(&endpoint.port().to_be_bytes());
    raw[107..115].copy_from_slice(&expiry.to_be_bytes());
    sign(&mut raw, key);
    raw
}

fn change_type(frame: &[u8], ty: u8, key: &[u8; 32]) -> Vec<u8> {
    let mut raw = frame.to_vec();
    raw[5] = ty;
    sign(&mut raw, key);
    raw
}

struct Pending {
    hello: Packet,
    endpoint: SocketAddr,
    challenge: Vec<u8>,
    at: u64,
    renewal: bool,
    fingerprint: [u8; 32],
    /// Retransmissions answered for this hello from this endpoint.
    retries: u32,
}

struct Completed {
    response: Packet,
    endpoint: SocketAddr,
    ack: Vec<u8>,
    at: u64,
    fingerprint: [u8; 32],
    /// Retransmissions answered for this response from this endpoint.
    retries: u32,
}

struct Binding {
    grant: [u8; 16],
    peer: u64,
    kind: u8,
    endpoint: SocketAddr,
    key_hash: [u8; 32],
    at: u64,
}

struct Seen {
    grant: [u8; 16],
    kind: u8,
    nonce: [u8; 16],
    endpoint: SocketAddr,
    at: u64,
    fingerprint: [u8; 32],
}

#[derive(Default)]
struct PreRate {
    at: u64,
    count: u32,
    ips: Vec<(IpAddr, u32)>,
    /// Frames from an endpoint already bound to the grant: their own budget, which frames
    /// from other endpoints (a replay included) cannot spend.
    bound: u32,
}

/// A grant's hellos in the current second: hellos from new endpoints and renewals from an
/// endpoint already bound to it are counted apart, so replays from new endpoints never use up
/// the bound endpoint's renewal.
#[derive(Clone, Copy)]
struct GrantRate {
    grant: [u8; 16],
    at: u64,
    fresh_hellos: u32,
    renewals: u32,
}

/// The grant authorities the host knows now (the live verified snapshot).
pub trait Lookup {
    fn authority(&self, grant: &[u8; 16]) -> Option<&Authority>;
}

/// FIUP counters (the `diag` event's `fiup`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    pub hello_game: u32,
    pub hello_hook: u32,
    pub response_game: u32,
    pub response_hook: u32,
    pub challenge_game: u32,
    pub challenge_hook: u32,
    pub ack_game: u32,
    pub ack_hook: u32,
    pub rejected: u32,
}

/// The host side of FIUP: bounded pending challenges, completed proofs, bindings and replay
/// memory. Time is the caller's monotonic ms; `unix` its trusted Unix seconds.
#[derive(Default)]
pub struct ProofHost {
    pending: Vec<Pending>,
    completed: Vec<Completed>,
    bindings: Vec<Binding>,
    /// (peer, kind, endpoint) of every binding: `allowed` rejects an unbound endpoint here.
    bound: HashSet<(u64, u8, SocketAddr)>,
    seen: Vec<Seen>,
    rates: Vec<GrantRate>,
    pre: Vec<([u8; 16], PreRate)>,
    rate_at: u64,
    pre_at: u64,
    new_count: u32,
    renew_count: u32,
    pub counters: Counters,
}

impl ProofHost {
    /// The most the proof tables can hold, every table at its cap (contract §8's worst case):
    /// each pending or completed handshake keeps two frames on the heap, each pre-rate entry
    /// its source list; the binding index is a hash set at twice its entries.
    pub fn worst_case_bytes() -> usize {
        use std::mem::size_of;
        let frame = FRAME_LEN + super::ALLOC_SLACK_BYTES;
        let pending = HANDSHAKES_MAX * (size_of::<Pending>() + 2 * frame);
        let completed = HANDSHAKES_MAX * (size_of::<Completed>() + 2 * frame);
        let bindings =
            HANDSHAKES_MAX * (size_of::<Binding>() + 2 * (size_of::<(u64, u8, SocketAddr)>() + 1));
        let seen = TABLE_MAX * size_of::<Seen>();
        let rates = TABLE_MAX * size_of::<GrantRate>();
        let pre = TABLE_MAX
            * (size_of::<([u8; 16], PreRate)>()
                + PRE_IPS_MAX * size_of::<(IpAddr, u32)>()
                + super::ALLOC_SLACK_BYTES);
        pending + completed + bindings + seen + rates + pre + 6 * super::ALLOC_SLACK_BYTES
    }
}

fn fresh(at: u64, now: u64, ms: u64) -> bool {
    now >= at && now - at < ms
}

fn bump(c: &mut u32) {
    *c = c.saturating_add(1);
}

impl std::fmt::Debug for ProofHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProofHost")
            .field("bindings", &self.bindings.len())
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl ProofHost {
    fn same_authority(l: &dyn Lookup, grant: &[u8; 16], fp: &[u8; 32], unix: i64) -> bool {
        l.authority(grant)
            .is_some_and(|a| a.expires > unix && a.fingerprint() == *fp && *fp != [0; 32])
    }

    /// Whether `endpoint` holds a fresh binding of any peer and kind (the public socket's
    /// routing, contract §5.2; authorization stays [`ProofHost::allowed`]'s).
    pub fn binds(&self, endpoint: SocketAddr, now: u64) -> bool {
        let endpoint = crate::stun::plain_endpoint(endpoint);
        self.bindings
            .iter()
            .any(|b| b.endpoint == endpoint && fresh(b.at, now, BINDING_MS))
    }

    fn reindex(&mut self) {
        self.bound.clear();
        self.bound
            .extend(self.bindings.iter().map(|b| (b.peer, b.kind, b.endpoint)));
    }

    /// Drops expired state. Runs on housekeeping, on a snapshot or kick, and before a verified
    /// frame is handled; never per game datagram (`allowed` checks what it reads itself).
    fn prune(&mut self, l: &dyn Lookup, now: u64, unix: i64) {
        let u = unix.max(0) as u64;
        let before = self.bindings.len();
        self.bindings.retain(|b| {
            fresh(b.at, now, BINDING_MS) && Self::same_authority(l, &b.grant, &b.key_hash, unix)
        });
        if self.bindings.len() != before {
            self.reindex();
        }
        self.pending.retain(|p| {
            fresh(p.at, now, PENDING_MS)
                && u64::from_be_bytes(p.challenge[107..115].try_into().unwrap_or([0; 8])) > u
                && Self::same_authority(l, &p.hello.grant, &p.fingerprint, unix)
        });
        self.completed.retain(|p| {
            fresh(p.at, now, PENDING_MS)
                && p.response.expires > u
                && Self::same_authority(l, &p.response.grant, &p.fingerprint, unix)
        });
        self.seen.retain(|p| {
            fresh(p.at, now, SEEN_MS) && Self::same_authority(l, &p.grant, &p.fingerprint, unix)
        });
        self.rates.retain(|r| fresh(r.at, now, 1000));
        self.pre.retain(|r| fresh(r.1.at, now, 1000));
        if !fresh(self.rate_at, now, 1000) {
            self.rate_at = now;
            self.new_count = 0;
            self.renew_count = 0;
        }
    }

    /// Drops what no longer matches the snapshot (a replaced or revoked grant).
    pub fn reconcile(&mut self, l: &dyn Lookup, now: u64, unix: i64) {
        self.prune(l, now, unix);
    }

    fn preliminary(&mut self, grant: [u8; 16], source: SocketAddr, now: u64) -> bool {
        let bound = self
            .bindings
            .iter()
            .any(|b| b.grant == grant && b.endpoint == source);
        let ip = crate::net::ip_key(source);
        if !fresh(self.pre_at, now, 1000) {
            self.pre.clear();
            self.pre_at = now;
        }
        if let Some(i) = self.pre.iter().position(|r| r.0 == grant)
            && !fresh(self.pre[i].1.at, now, 1000)
        {
            self.pre.remove(i);
        }
        let i = match self.pre.iter().position(|r| r.0 == grant) {
            Some(i) => i,
            None => {
                if self.pre.len() >= TABLE_MAX {
                    return false;
                }
                self.pre.push((
                    grant,
                    PreRate {
                        at: now,
                        ..PreRate::default()
                    },
                ));
                self.pre.len() - 1
            }
        };
        let rate = &mut self.pre[i].1;
        if bound {
            if rate.bound >= 64 {
                return false;
            }
            rate.bound += 1;
            return true;
        }
        let have = rate.ips.iter().find(|x| x.0 == ip).map_or(0, |x| x.1);
        if rate.count >= 64 || have >= 16 || (rate.ips.len() >= PRE_IPS_MAX && have == 0) {
            return false;
        }
        rate.count += 1;
        match rate.ips.iter_mut().find(|x| x.0 == ip) {
            Some(x) => x.1 += 1,
            None => rate.ips.push((ip, 1)),
        }
        true
    }

    /// Whether `endpoint` is bound to `peer` for `kind` now: an indexed miss for any unbound
    /// endpoint; a bound one is checked for its 45 s and its authority's cached fingerprint (no
    /// hashing, no pruning: this runs per game datagram).
    pub fn allowed(
        &self,
        l: &dyn Lookup,
        peer: u64,
        kind: u8,
        endpoint: SocketAddr,
        now: u64,
        unix: i64,
    ) -> bool {
        let endpoint = crate::stun::plain_endpoint(endpoint);
        if !self.bound.contains(&(peer, kind, endpoint)) {
            return false;
        }
        self.bindings.iter().any(|b| {
            b.peer == peer
                && b.kind == kind
                && b.endpoint == endpoint
                && fresh(b.at, now, BINDING_MS)
                && Self::same_authority(l, &b.grant, &b.key_hash, unix)
        })
    }

    /// A FIUP datagram from `source`: the frame to answer with, if any.
    pub fn receive(
        &mut self,
        l: &dyn Lookup,
        data: &[u8],
        source: SocketAddr,
        now: u64,
        unix: i64,
    ) -> Option<Vec<u8>> {
        let p = parse(data);
        if let Some(p) = p.as_ref().filter(|p| p.ty == 1 || p.ty == 3) {
            let c = &mut self.counters;
            match (p.ty, p.kind) {
                (1, 0) => bump(&mut c.hello_game),
                (1, _) => bump(&mut c.hello_hook),
                (_, 0) => bump(&mut c.response_game),
                _ => bump(&mut c.response_hook),
            }
        }
        let answer = self.receive_core(l, p, source, now, unix);
        let c = &mut self.counters;
        match answer.as_deref() {
            Some(a) if a.len() == FRAME_LEN => match (a[5], a[6]) {
                (2, 0) => bump(&mut c.challenge_game),
                (2, _) => bump(&mut c.challenge_hook),
                (_, 0) => bump(&mut c.ack_game),
                _ => bump(&mut c.ack_hook),
            },
            _ => bump(&mut c.rejected),
        }
        answer
    }

    fn receive_core(
        &mut self,
        l: &dyn Lookup,
        p: Option<Packet>,
        source: SocketAddr,
        now: u64,
        unix: i64,
    ) -> Option<Vec<u8>> {
        let p = p.filter(|p| p.ty == 1 || p.ty == 3)?;
        let source = crate::stun::plain_endpoint(source);
        if source.port() == 0 || !unicast(source.ip()) {
            return None;
        }
        let authority = l.authority(&p.grant)?.clone();
        if authority.expires <= unix
            || !same_context(&p, &authority.header)
            || !self.preliminary(p.grant, source, now)
            || !verify(&p, &authority.key)
        {
            return None;
        }
        self.prune(l, now, unix);
        let fingerprint = authority.fingerprint();
        let u = unix.max(0) as u64;
        if p.ty == 1 {
            if let Some(c) = self
                .pending
                .iter_mut()
                .find(|x| x.endpoint == source && x.hello.raw == p.raw)
            {
                c.retries += 1;
                return (c.retries <= RETRIES).then(|| c.challenge.clone());
            }
            let nonce: [u8; 16] = p.raw[56..72].try_into().ok()?;
            // a consumed nonce cannot start a new lifetime
            if self.completed.iter().any(|x| {
                x.response.grant == p.grant
                    && x.response.kind == p.kind
                    && x.response.raw[56..72] == nonce
            }) {
                return None;
            }
            if self.seen.len() >= TABLE_MAX
                || self.seen.iter().any(|x| {
                    x.grant == p.grant
                        && x.kind == p.kind
                        && x.endpoint == source
                        && x.nonce == nonce
                })
            {
                return None;
            }
            let renewal = self
                .bindings
                .iter()
                .any(|b| b.grant == p.grant && b.endpoint == source);
            if self
                .bindings
                .iter()
                .any(|b| b.peer != p.peer && b.endpoint == source)
            {
                return None;
            }
            let known = self.rates.iter().position(|r| r.grant == p.grant);
            let rate = known.map_or(
                GrantRate {
                    grant: p.grant,
                    at: now,
                    fresh_hellos: 0,
                    renewals: 0,
                },
                |i| self.rates[i],
            );
            let established = self.bindings.iter().any(|b| b.grant == p.grant);
            let over = if renewal {
                rate.renewals >= 2
            } else {
                rate.fresh_hellos >= 2 || (established && rate.fresh_hellos >= 1)
            };
            if over || (self.rates.len() >= TABLE_MAX && known.is_none()) {
                return None;
            }
            if if renewal {
                self.renew_count >= 32
            } else {
                self.new_count >= 32
            } {
                return None;
            }
            // a grant's pending slots: two for new endpoints, two for renewals, never shared
            if self.pending.len() >= HANDSHAKES_MAX
                || (!renewal && self.pending.iter().filter(|x| !x.renewal).count() >= 32)
                || self
                    .pending
                    .iter()
                    .filter(|x| {
                        x.hello.grant == p.grant && x.hello.kind == p.kind && x.renewal == renewal
                    })
                    .count()
                    >= 2
                || (!renewal
                    && established
                    && self
                        .pending
                        .iter()
                        .any(|x| x.hello.grant == p.grant && x.hello.kind == p.kind && !x.renewal))
            {
                return None;
            }
            let entry = GrantRate {
                grant: p.grant,
                at: rate.at,
                fresh_hellos: rate.fresh_hellos + u32::from(!renewal),
                renewals: rate.renewals + u32::from(renewal),
            };
            match known {
                Some(i) => self.rates[i] = entry,
                None => self.rates.push(entry),
            }
            if renewal {
                self.renew_count += 1;
            } else {
                self.new_count += 1;
            }
            let expiry = (unix + 5).min(authority.expires).max(0) as u64;
            let ch = challenge(&p, source, expiry, &authority.key);
            self.seen.push(Seen {
                grant: p.grant,
                kind: p.kind,
                nonce,
                endpoint: source,
                at: now,
                fingerprint,
            });
            self.pending.push(Pending {
                hello: p,
                endpoint: source,
                challenge: ch.clone(),
                at: now,
                renewal,
                fingerprint,
                retries: 0,
            });
            return Some(ch);
        }
        if let Some(c) = self
            .completed
            .iter_mut()
            .find(|x| x.endpoint == source && x.response.raw == p.raw)
        {
            c.retries += 1;
            return (c.retries <= RETRIES).then(|| c.ack.clone());
        }
        let at = self.pending.iter().position(|x| {
            x.endpoint == source
                && x.hello.grant == p.grant
                && x.hello.kind == p.kind
                && x.challenge[8..115] == p.raw[8..115]
        })?;
        if p.observed != Some(source)
            || p.expires <= u
            || self
                .bindings
                .iter()
                .any(|b| b.peer != p.peer && b.endpoint == source)
        {
            return None;
        }
        if self.bindings.len() >= HANDSHAKES_MAX
            && !self
                .bindings
                .iter()
                .any(|b| b.grant == p.grant && b.kind == p.kind)
        {
            return None;
        }
        self.pending.remove(at);
        self.bindings
            .retain(|b| !(b.grant == p.grant && b.kind == p.kind));
        self.bindings.push(Binding {
            grant: p.grant,
            peer: p.peer,
            kind: p.kind,
            endpoint: source,
            key_hash: fingerprint,
            at: now,
        });
        self.reindex();
        let ack = change_type(&p.raw, 4, &authority.key);
        if self.completed.len() == HANDSHAKES_MAX {
            self.completed.remove(0);
        }
        self.completed.push(Completed {
            response: p,
            endpoint: source,
            ack: ack.clone(),
            at: now,
            fingerprint,
            retries: 0,
        });
        Some(ack)
    }

    pub fn clear(&mut self) {
        let counters = self.counters;
        *self = ProofHost::default();
        self.counters = counters;
    }
}

/// Test support: a joiner's frames (the hook's side), signed with the grant's key.
pub mod joiner {
    use super::*;

    /// A hello from a joiner (kind 0 game, 1 hook) with its 16-byte nonce.
    pub fn hello(header: &[u8; 56], kind: u8, nonce: [u8; 16], key: &[u8; 32]) -> Vec<u8> {
        let mut raw = vec![0u8; HELLO_LEN];
        raw[..56].copy_from_slice(header);
        raw[5] = 1;
        raw[6] = kind;
        raw[56..72].copy_from_slice(&nonce);
        sign(&mut raw, key);
        raw
    }

    /// The response to a host's challenge.
    pub fn response(challenge: &[u8], key: &[u8; 32]) -> Vec<u8> {
        change_type(challenge, 3, key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_official_host_tag_vectors() {
        // WORLDS_OFFICIAL.md §3; DEDICATED_SERVER.md §6.3 lists the same vectors
        assert_eq!(official_host_tag("local-mac"), 0xfda2_331c_be2b_d82e);
        assert_eq!(official_host_tag("sao-1"), 0xde2d_c048_516f_ba57);
        for vm in [
            "a",
            "local-mac",
            "sao-1",
            "x-0123456789abcdefghijklmnopqrst",
        ] {
            let tag = official_host_tag(vm);
            assert!(tag != 0 && tag >> 63 == 1, "{vm}");
        }
        assert_ne!(official_host_tag("sao-1"), official_host_tag("sao-2"));
    }
}
