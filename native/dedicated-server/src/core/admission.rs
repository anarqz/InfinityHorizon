// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! A world's admission (contract §6.2, normative; ported from the C# `WorldRelayAdmission`,
//! client f81d09c): the trusted clock, the 45 s host lease (`world.context` only), the verified
//! snapshot of the punches (`world.admission`), revocations, FIUP, the network hints, and the
//! per-packet decision the engines ask. Game-agnostic: a game module sees it through
//! [`super::game::Admit`]. No policy never means allow-all: before a context and after any
//! clear, everybody but nobody is denied.

use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex, MutexGuard},
};

use serde_json::Value;

use super::{
    fiup::{self, Authority, Lookup, ProofHost},
    grant::{self, Context},
    keys::KeyRing,
    legs::LegSpec,
};

/// The host lease (`HostLease`).
pub const HOST_LEASE_MS: u64 = 45_000;
/// Revocations last a grant's lifetime plus the clock skew.
const REVOKED_MS: u64 = ((grant::MAX_LIFETIME_SECS + grant::FUTURE_SKEW_SECS) * 1000) as u64;
const REVOKED_MAX: usize = 4096;
const HINTS_MAX: usize = 64;

/// §6.2 "Time": `max(the serverTimeMs-derived clock, the monotonic clock since the first
/// context)`, never moving back (`EffectiveNow`). Unix milliseconds.
#[derive(Debug, Clone, Default)]
pub struct TrustedClock {
    origin: Option<(u64, u64)>,
    wall: Option<(u64, u64)>,
    last: u64,
}

impl TrustedClock {
    pub fn observe(&mut self, server_ms: u64, mono: u64) {
        if self.origin.is_none() {
            self.origin = Some((server_ms, mono));
        }
        self.wall = Some((server_ms, mono));
    }

    pub fn now(&mut self, mono: u64) -> Option<u64> {
        let (o_ms, o_mono) = self.origin?;
        let monotonic = o_ms + mono.saturating_sub(o_mono);
        let wall = self
            .wall
            .map_or(0, |(w_ms, w_mono)| w_ms + mono.saturating_sub(w_mono));
        self.last = self.last.max(monotonic.max(wall));
        Some(self.last)
    }
}

/// The world's identity and rules, fixed at `world.open`.
#[derive(Debug, Clone)]
pub struct Rules {
    pub world_id: String,
    /// The host's decimal XUID (`player`), or `official:<vmId>`.
    pub host: String,
    /// 0 for an `official` host.
    pub host_xuid: u64,
    pub is_player: bool,
    pub require_proof: bool,
    pub dev_lan_peers: bool,
    /// Public: every leg goes to the co-located relay (contract §5.2).
    pub relay: Option<SocketAddr>,
}

#[derive(Debug, Clone)]
struct Entry {
    grant: String,
    /// Unix seconds.
    expires: i64,
    lifetime_ms: u64,
    started: u64,
}

impl Entry {
    fn live(&self, unix: i64, mono: u64) -> bool {
        unix < self.expires && mono.saturating_sub(self.started) < self.lifetime_ms
    }
}

#[derive(Debug)]
struct ProofGrant {
    peer: u64,
    entry: Entry,
    authority: Authority,
}

#[derive(Debug, Default)]
struct Snapshot {
    entries: HashMap<(u64, SocketAddr), Entry>,
    lan: HashMap<u64, Entry>,
    /// By grant id: the per-datagram decision finds a grant's authority in O(1).
    proofs: HashMap<[u8; 16], ProofGrant>,
}

/// One punch as the hints and kicks read it again.
#[derive(Debug, Clone)]
struct PunchInfo {
    xuid: String,
    grant: String,
    kicked: bool,
    candidates: Option<Value>,
}

/// What a snapshot produced (the `world.admission` reply and the world's follow-ups).
#[derive(Debug, Default)]
pub struct Outcome {
    pub admitted: usize,
    pub proof_grants: usize,
    pub legs: Vec<LegSpec>,
    /// XUIDs of `kicked: true` punches (their peers are dropped).
    pub kicked: Vec<u64>,
    /// `(grant, revision, candidates)` to punch (the NetworkHints rules).
    pub hints: Vec<([u8; 16], u32, Vec<SocketAddr>)>,
    /// CR-S67: each XUID's latest signed `voiceBlockedUntil` (0: none).
    pub voice_blocks: BTreeMap<u64, i64>,
}

/// A world's admission state.
#[derive(Debug)]
pub struct Gate {
    rules: Rules,
    world_bytes: [u8; 16],
    pub clock: TrustedClock,
    pub has_context: bool,
    /// (until, trusted Unix ms; started, monotonic ms)
    lease: Option<(u64, u64)>,
    snapshot: Option<Snapshot>,
    revoked: HashMap<String, u64>,
    punches: Vec<PunchInfo>,
    proof: ProofHost,
    admitted: usize,
}

/// The snapshot's live FIUP authorities.
struct LiveProofs<'a> {
    snapshot: Option<&'a Snapshot>,
    host_live: bool,
    unix: i64,
    mono: u64,
}

impl Lookup for LiveProofs<'_> {
    fn authority(&self, grant: &[u8; 16]) -> Option<&Authority> {
        if !self.host_live {
            return None;
        }
        self.snapshot?
            .proofs
            .get(grant)
            .filter(|p| p.entry.live(self.unix, self.mono))
            .map(|p| &p.authority)
    }
}

fn unicast(ip: IpAddr) -> bool {
    fiup::unicast(ip)
}

/// RFC 1918 (IPv4-mapped unmapped first).
fn private_ip(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v => v,
    };
    matches!(ip, IpAddr::V4(v) if v.is_private())
}

fn loopback(e: SocketAddr) -> bool {
    crate::stun::canonical(e).ip().is_loopback()
}

/// Unix milliseconds of an RFC 3339 instant (`Z` or a numeric offset, optional fraction).
pub fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let t = s.get(r)?;
        t.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| t.parse().ok())?
    };
    let (y, mo, d, h, mi, se) = (
        num(0..4)?,
        num(5..7)?,
        num(8..10)?,
        num(11..13)?,
        num(14..16)?,
        num(17..19)?,
    );
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac_ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let digits = &s[start..i.min(start + 3)];
        frac_ms = digits.parse::<i64>().ok()? * 10i64.pow(3 - digits.len() as u32);
    }
    let offset_min = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        c @ (b'+' | b'-') if i + 6 == b.len() && b[i + 3] == b':' => {
            let oh = num(i + 1..i + 3)?;
            let om = num(i + 4..i + 6)?;
            let v = oh * 60 + om;
            if *c == b'-' { -v } else { v }
        }
        _ => return None,
    };
    // days from the civil date (proleptic Gregorian)
    let (yy, mm) = if mo <= 2 {
        (y - 1, mo + 9)
    } else {
        (y, mo - 3)
    };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * mm + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + se - offset_min * 60;
    Some(secs * 1000 + frac_ms)
}

/// `Punch.candidates.candidates` as endpoints (UDP_SOCKET_PROOF.md §6): at most 5, at most one
/// `public` and four `lan`, unicast, canonical text, no duplicate; `None` when any fails.
fn candidate_endpoints(list: &Value) -> Option<Vec<SocketAddr>> {
    let list = list.as_array()?;
    if list.len() > 5 {
        return None;
    }
    let kind = |c: &Value| c.get("kind").and_then(Value::as_str).map(str::to_string);
    if list
        .iter()
        .filter(|c| kind(c).as_deref() == Some("public"))
        .count()
        > 1
        || list
            .iter()
            .filter(|c| kind(c).as_deref() == Some("lan"))
            .count()
            > 4
    {
        return None;
    }
    let mut out = Vec::new();
    for c in list {
        if !matches!(kind(c).as_deref(), Some("public" | "lan")) {
            return None;
        }
        let port = u16::try_from(c.get("port")?.as_u64()?)
            .ok()
            .filter(|p| *p > 0)?;
        let text = c.get("ip")?.as_str()?;
        let ip: IpAddr = text.parse().ok()?;
        let ok = match ip {
            IpAddr::V4(v) => (1..224).contains(&v.octets()[0]) && v != Ipv4Addr::BROADCAST,
            IpAddr::V6(v) => {
                v.to_ipv4_mapped().is_none() && !v.is_unspecified() && !v.is_multicast()
            }
        };
        if !ok || ip.to_string() != text {
            return None;
        }
        let e = SocketAddr::new(ip, port);
        if out.contains(&e) {
            return None;
        }
        out.push(e);
    }
    Some(out)
}

impl Gate {
    pub fn new(rules: Rules) -> Gate {
        let world_bytes = grant::uuid_bytes(&rules.world_id).unwrap_or([0; 16]);
        Gate {
            rules,
            world_bytes,
            clock: TrustedClock::default(),
            has_context: false,
            lease: None,
            snapshot: None,
            revoked: HashMap::new(),
            punches: Vec::new(),
            proof: ProofHost::default(),
            admitted: 0,
        }
    }

    pub fn rules(&self) -> &Rules {
        &self.rules
    }

    /// The lease's end (trusted Unix ms), when there is one.
    pub fn lease_until(&self) -> Option<u64> {
        self.lease.map(|l| l.0)
    }

    /// Trusted Unix ms (0 before the first context).
    fn now_ms(&mut self, mono: u64) -> u64 {
        self.clock.now(mono).unwrap_or(0)
    }

    pub fn host_live(&mut self, mono: u64) -> bool {
        let now = self.now_ms(mono);
        self.lease.is_some_and(|(until, started)| {
            mono >= started && mono - started < HOST_LEASE_MS && now < until
        })
    }

    /// `world.context` passed its checks: the clock and a new 45 s lease; the leaseUntilMs.
    pub fn context(&mut self, server_ms: u64, mono: u64) -> u64 {
        self.clock.observe(server_ms, mono);
        let now = self.clock.now(mono).unwrap_or(server_ms);
        let until = now + HOST_LEASE_MS;
        self.has_context = true;
        self.lease = Some((until, mono));
        until
    }

    /// Clears everything (fail closed): no lease, no snapshot, no proof, keys zeroed.
    pub fn clear(&mut self) {
        self.has_context = false;
        self.lease = None;
        if let Some(mut s) = self.snapshot.take() {
            for p in s.proofs.values_mut() {
                p.authority.zero();
            }
        }
        self.punches.clear();
        self.proof.clear();
        self.admitted = 0;
    }

    fn revoke_grant(&mut self, grant: &str, mono: u64) {
        if self.revoked.len() < REVOKED_MAX && grant::uuid(grant) {
            self.revoked.insert(grant.to_string(), mono);
        }
    }

    /// `world.admission`: verifies the punches against `ring` and replaces the snapshot at once
    /// (the old one serves until then; its keys are zeroed after the swap).
    pub fn admission(
        &mut self,
        punches: &[Value],
        ring: &KeyRing,
        server_ms: u64,
        mono: u64,
    ) -> Outcome {
        self.clock.observe(server_ms, mono);
        let now_ms = self.now_ms(mono);
        let unix = (now_ms / 1000) as i64;
        self.revoked
            .retain(|_, at| mono.saturating_sub(*at) < REVOKED_MS);
        let mut out = Outcome::default();
        let infos: Vec<PunchInfo> = punches
            .iter()
            .map(|p| PunchInfo {
                xuid: p
                    .get("xuid")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                grant: p
                    .get("grantId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                kicked: p.get("kicked").and_then(Value::as_bool).unwrap_or(false),
                candidates: p.get("candidates").filter(|c| c.is_object()).cloned(),
            })
            .collect();
        for k in infos.iter().filter(|p| p.kicked) {
            let g = k.grant.clone();
            self.revoke_grant(&g, mono);
            if let Some(x) = grant::xuid(&k.xuid)
                .then(|| k.xuid.parse::<u64>().ok())
                .flatten()
                && !out.kicked.contains(&x)
            {
                out.kicked.push(x);
            }
        }
        let mut snap = Snapshot::default();
        let host_text = self.rules.host.clone();
        let world_id = self.rules.world_id.clone();
        for (p, info) in punches.iter().zip(&infos) {
            let duplicate = infos.iter().filter(|o| o.grant == info.grant).count() > 1;
            let Some(relay) = p.get("relay").filter(|r| r.is_object()) else {
                continue;
            };
            let xuid = info
                .xuid
                .parse::<u64>()
                .ok()
                .filter(|_| grant::xuid(&info.xuid));
            let Some(xuid) = xuid else { continue };
            if info.kicked
                || duplicate
                || self.revoked.contains_key(&info.grant)
                || self.revoked.len() >= REVOKED_MAX
                || xuid == self.rules.host_xuid
            {
                continue;
            }
            let observed = p.get("endpoint").and_then(|e| {
                let ip: IpAddr = e.get("ip")?.as_str()?.parse().ok()?;
                let port = u16::try_from(e.get("port")?.as_u64()?)
                    .ok()
                    .filter(|p| *p > 0)?;
                unicast(ip).then(|| crate::stun::canonical(SocketAddr::new(ip, port)))
            });
            let mut proof_key: Option<[u8; 32]> = relay.get("udpProof").and_then(|u| {
                let key = u.get("key")?.as_str()?;
                if u.get("version")?.as_u64()? != 1 || key.len() != 44 {
                    return None;
                }
                super::keys::decode_base64(key)?.try_into().ok()
            });
            if observed.is_none() && !self.rules.dev_lan_peers && proof_key.is_none() {
                continue;
            }
            let (claims_b64, sig_b64) = (
                relay.get("claims").and_then(Value::as_str).unwrap_or(""),
                relay.get("signature").and_then(Value::as_str).unwrap_or(""),
            );
            let ctx = Context {
                world: &world_id,
                host_xuid: &host_text,
                peer_xuid: &info.xuid,
                role: "host",
                grant: Some(&info.grant),
            };
            let (claims, raw_claims, raw_sig) =
                match grant::verify(claims_b64, sig_b64, ring, unix, ctx) {
                    Ok(v) => v,
                    Err(_) => {
                        if let Some(k) = proof_key.as_mut() {
                            k.fill(0);
                        }
                        continue;
                    }
                };
            out.admitted += 1;
            let block = claims.voice_blocked_until.unwrap_or(0);
            let e = out.voice_blocks.entry(xuid).or_insert(block);
            *e = (*e).max(block);
            let lifetime = (claims.exp.saturating_mul(1000) - now_ms as i64).max(0) as u64;
            let entry = Entry {
                grant: claims.grant.clone(),
                expires: claims.exp,
                lifetime_ms: lifetime.min(HOST_LEASE_MS),
                started: mono,
            };
            let grant_bytes = grant::uuid_bytes(&claims.grant).unwrap_or([0; 16]);
            if let Some(key) = proof_key {
                let authority = Authority::new(
                    fiup::header(grant_bytes, self.world_bytes, self.rules.host_xuid, xuid),
                    key,
                    claims.exp,
                );
                if let Some(mut old) = snap.proofs.insert(
                    grant_bytes,
                    ProofGrant {
                        peer: xuid,
                        entry: entry.clone(),
                        authority,
                    },
                ) {
                    old.authority.zero();
                }
            }
            if let Some(o) = observed {
                snap.entries.insert((xuid, o), entry.clone());
            }
            // CR-S57: only the relay the verified grant names is contacted (public: the
            // co-located relay, §5.2)
            let leg_relay = match self.rules.relay {
                Some(r) => Some(r),
                None => claims.relay.as_ref().and_then(|r| {
                    r.addr
                        .parse::<Ipv4Addr>()
                        .ok()
                        .map(|a| SocketAddr::new(IpAddr::V4(a), r.port))
                }),
            };
            if let Some(relay) = leg_relay {
                out.legs.push(LegSpec {
                    grant: grant_bytes,
                    claims: raw_claims,
                    signature: raw_sig,
                    relay,
                    forward: self.rules.relay.is_some()
                        || p.get("transport").and_then(Value::as_str) == Some("relay"),
                });
            }
            if self.rules.dev_lan_peers {
                snap.lan.insert(xuid, entry);
            }
        }
        out.proof_grants = snap.proofs.len();
        self.admitted = out.admitted;
        let old = self.snapshot.replace(snap);
        self.punches = infos;
        let unix_now = unix;
        let host_live = self.host_live(mono);
        let lookup = LiveProofs {
            snapshot: self.snapshot.as_ref(),
            host_live,
            unix: unix_now,
            mono,
        };
        self.proof.reconcile(&lookup, mono, unix_now);
        if let Some(mut old) = old {
            for p in old.proofs.values_mut() {
                p.authority.zero();
            }
        }
        out.hints = self.hints(mono);
        out
    }

    /// The punches' candidates to punch (`NetworkHints`, §6.2): at most 64, not kicked,
    /// revision > 0, a live FIUP authority, unexpired, at most 30 s long, not from the future.
    fn hints(&mut self, mono: u64) -> Vec<([u8; 16], u32, Vec<SocketAddr>)> {
        let now = self.now_ms(mono) as i64;
        let host_live = self.host_live(mono);
        let lookup = LiveProofs {
            snapshot: self.snapshot.as_ref(),
            host_live,
            unix: now / 1000,
            mono,
        };
        let mut out = Vec::new();
        for p in &self.punches {
            if out.len() >= HINTS_MAX {
                break;
            }
            let Some(c) = p.candidates.as_ref().filter(|_| !p.kicked) else {
                continue;
            };
            let Some(revision) = c
                .get("revision")
                .and_then(Value::as_u64)
                .and_then(|r| u32::try_from(r).ok())
                .filter(|r| *r > 0)
            else {
                continue;
            };
            let Some(grant) = grant::uuid_bytes(&p.grant) else {
                continue;
            };
            if lookup.authority(&grant).is_none() {
                continue;
            }
            let (Some(generated), Some(expires)) = (
                c.get("generatedAt")
                    .and_then(Value::as_str)
                    .and_then(rfc3339_ms),
                c.get("expiresAt")
                    .and_then(Value::as_str)
                    .and_then(rfc3339_ms),
            ) else {
                continue;
            };
            if expires <= now || expires - generated > 30_000 || generated > now + 1000 {
                continue;
            }
            if let Some(eps) = c.get("candidates").and_then(candidate_endpoints) {
                out.push((grant, revision, eps));
            }
        }
        out
    }

    /// The decision for (xuid, endpoint, kind 0 game / 1 hook), in §6.2's order.
    pub fn allowed(&mut self, xuid: u64, endpoint: SocketAddr, kind: u8, mono: u64) -> bool {
        let endpoint = crate::stun::canonical(endpoint);
        if self.rules.host_xuid != 0 && xuid == self.rules.host_xuid {
            return self.rules.is_player && self.host_live(mono) && loopback(endpoint);
        }
        if self.snapshot.is_none() || !self.host_live(mono) {
            return false;
        }
        let now = self.now_ms(mono);
        let unix = (now / 1000) as i64;
        let Gate {
            snapshot, proof, ..
        } = self;
        let lookup = LiveProofs {
            snapshot: snapshot.as_ref(),
            host_live: true,
            unix,
            mono,
        };
        if proof.allowed(&lookup, xuid, kind, endpoint, mono, unix) {
            return true;
        }
        if self.rules.require_proof {
            return false;
        }
        let Some(s) = self.snapshot.as_ref() else {
            return false;
        };
        if s.entries
            .get(&(xuid, endpoint))
            .is_some_and(|e| e.live(unix, mono))
        {
            return true;
        }
        self.rules.dev_lan_peers
            && private_ip(endpoint.ip())
            && s.lan.get(&xuid).is_some_and(|e| e.live(unix, mono))
    }

    /// Whether `xuid` holds a live grant its endpoint can prove (a held OpenConnection waits).
    pub fn proof_expected(&mut self, xuid: u64, mono: u64) -> bool {
        if xuid == self.rules.host_xuid || !self.host_live(mono) {
            return false;
        }
        let unix = (self.now_ms(mono) / 1000) as i64;
        self.snapshot.as_ref().is_some_and(|s| {
            s.proofs
                .values()
                .any(|p| p.peer == xuid && p.entry.live(unix, mono))
        })
    }

    /// A FIUP datagram: the answer to send back the same way, if any.
    pub fn receive_proof(&mut self, data: &[u8], source: SocketAddr, mono: u64) -> Option<Vec<u8>> {
        let host_live = self.host_live(mono);
        let unix = (self.now_ms(mono) / 1000) as i64;
        let Gate {
            snapshot, proof, ..
        } = self;
        let lookup = LiveProofs {
            snapshot: snapshot.as_ref(),
            host_live,
            unix,
            mono,
        };
        proof.receive(&lookup, data, source, mono, unix)
    }

    /// Housekeeping (once a second): drops expired FIUP state, off the datagram path.
    pub fn housekeep(&mut self, mono: u64) {
        let host_live = self.host_live(mono);
        let unix = (self.now_ms(mono) / 1000) as i64;
        let Gate {
            snapshot, proof, ..
        } = self;
        let lookup = LiveProofs {
            snapshot: snapshot.as_ref(),
            host_live,
            unix,
            mono,
        };
        proof.reconcile(&lookup, mono, unix);
    }

    pub fn fiup_counters(&self) -> fiup::Counters {
        self.proof.counters
    }

    /// `world.kick`: revokes every grant of `xuid` in the snapshot for 90 s and zeroes its FIUP
    /// keys (its peers are dropped by the caller).
    pub fn revoke(&mut self, xuid: u64, mono: u64) {
        let Some(mut s) = self.snapshot.take() else {
            return;
        };
        let grants: Vec<String> = s
            .entries
            .iter()
            .filter(|(k, _)| k.0 == xuid)
            .map(|(_, e)| e.grant.clone())
            .chain(s.lan.get(&xuid).map(|e| e.grant.clone()))
            .collect();
        for g in grants {
            self.revoke_grant(&g, mono);
        }
        s.entries.retain(|k, _| k.0 != xuid);
        s.lan.remove(&xuid);
        let id = xuid.to_string();
        let punched: Vec<String> = self
            .punches
            .iter()
            .filter(|p| p.xuid == id)
            .map(|p| p.grant.clone())
            .collect();
        for g in punched {
            self.revoke_grant(&g, mono);
        }
        for p in s.proofs.values_mut().filter(|p| p.peer == xuid) {
            p.authority.zero();
        }
        s.proofs.retain(|_, p| p.peer != xuid);
        self.snapshot = Some(s);
        let host_live = self.host_live(mono);
        let unix = (self.now_ms(mono) / 1000) as i64;
        let Gate {
            snapshot, proof, ..
        } = self;
        let lookup = LiveProofs {
            snapshot: snapshot.as_ref(),
            host_live,
            unix,
            mono,
        };
        proof.reconcile(&lookup, mono, unix);
    }

    /// The live grants of the snapshot (`world.state.admission.grants`).
    pub fn grants(&self) -> usize {
        if self.snapshot.is_some() {
            self.admitted
        } else {
            0
        }
    }
}

/// The gate shared by a world and its engines (one loop thread; the lock is never contended).
#[derive(Debug, Clone)]
pub struct Shared(pub Arc<Mutex<Gate>>);

impl Shared {
    pub fn new(rules: Rules) -> Shared {
        Shared(Arc::new(Mutex::new(Gate::new(rules))))
    }

    pub fn lock(&self) -> MutexGuard<'_, Gate> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl super::game::Admit for Shared {
    fn allowed(&self, xuid: u64, from: SocketAddr, kind: u8, mono: u64) -> bool {
        self.lock().allowed(xuid, from, kind, mono)
    }

    fn proof_expected(&self, xuid: u64, mono: u64) -> bool {
        self.lock().proof_expected(xuid, mono)
    }
}
