// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! A world's admission (contract §6.2, normative; ported from the C# `WorldRelayAdmission`,
//! client f81d09c): the trusted clock, the 45 s host lease (`world.context` only), the verified
//! snapshot of the punches (`world.admission`), revocations, FIUP, and the per-packet decision
//! the engines ask. Game-agnostic: a game module sees it through [`super::game::Admit`]. No
//! policy never means allow-all: before a context and after any clear, everybody is denied.
//!
//! An official world (WORLDS_OFFICIAL.md §3, §6) has no player host and always requires proof:
//! a joiner is admitted only once its socket proved its grant with FIUP (UDP_SOCKET_PROOF.md),
//! never by an observed endpoint alone.

use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex, MutexGuard},
};

use serde_json::Value;

use super::{
    fiup::{self, Authority, Lookup, ProofHost},
    grant::{self, Context},
    keys::KeyRing,
};

/// The host lease (`HostLease`).
pub const HOST_LEASE_MS: u64 = 45_000;
/// Revocations last a grant's lifetime plus the clock skew.
const REVOKED_MS: u64 = ((grant::MAX_LIFETIME_SECS + grant::FUTURE_SKEW_SECS) * 1000) as u64;
const REVOKED_MAX: usize = 4096;

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

/// The world's identity, fixed at `world.open`.
#[derive(Debug, Clone)]
pub struct Rules {
    pub world_id: String,
    /// The official host's VM id (`world.open`'s `host.vmId`).
    pub vm_id: String,
}

impl Rules {
    /// The host as the grants name it: `official:<vmId>` (WORLDS_OFFICIAL.md §3).
    pub fn host(&self) -> String {
        grant::official_host(&self.vm_id)
    }

    /// The FIUP header's host field (UDP_SOCKET_PROOF.md §2): the official host tag
    /// (WORLDS_OFFICIAL.md §3).
    pub fn proof_host(&self) -> u64 {
        fiup::official_host_tag(&self.vm_id)
    }
}

#[derive(Debug, Clone)]
struct Entry {
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
    /// By grant id: the per-datagram decision finds a grant's authority in O(1).
    proofs: HashMap<[u8; 16], ProofGrant>,
}

/// One punch as the kicks read it again.
#[derive(Debug, Clone)]
struct PunchInfo {
    xuid: String,
    grant: String,
    kicked: bool,
}

/// What a snapshot produced (the `world.admission` reply and the world's follow-ups).
#[derive(Debug, Default)]
pub struct Outcome {
    pub admitted: usize,
    pub proof_grants: usize,
    /// XUIDs of `kicked: true` punches (their peers are dropped).
    pub kicked: Vec<u64>,
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

impl Gate {
    /// The most a world's gate holds on its own: the revoked grant ids at their cap (a UUID
    /// string each, in a hash map at twice its entries) and the proof tables. The snapshot and
    /// the punches are the controller's own bounded input.
    pub fn worst_case_bytes() -> usize {
        let revoked_entry = std::mem::size_of::<(String, u64)>() + 36 + super::ALLOC_SLACK_BYTES;
        2 * REVOKED_MAX * revoked_entry + ProofHost::worst_case_bytes()
    }
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
        let host_text = self.rules.host();
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
            {
                continue;
            }
            // a punch counts with an observed unicast endpoint or a FIUP key (contract §6.2); only
            // a FIUP proof ever admits to an official world
            let observed = p.get("endpoint").is_some_and(|e| {
                let ip = e
                    .get("ip")
                    .and_then(Value::as_str)
                    .and_then(|t| t.parse::<IpAddr>().ok());
                let port = e
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|p| *p > 0);
                ip.is_some_and(unicast) && port.is_some()
            });
            let mut proof_key: Option<[u8; 32]> = relay.get("udpProof").and_then(|u| {
                let key = u.get("key")?.as_str()?;
                if u.get("version")?.as_u64()? != 1 || key.len() != 44 {
                    return None;
                }
                super::keys::decode_base64(key)?.try_into().ok()
            });
            if !observed && proof_key.is_none() {
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
                // an official world has no host-role grant (WORLDS_OFFICIAL.md §3): its punches
                // carry the joiners' own grants
                role: "joiner",
                grant: Some(&info.grant),
            };
            let (claims, _, _) = match grant::verify(claims_b64, sig_b64, ring, unix, ctx) {
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
                expires: claims.exp,
                lifetime_ms: lifetime.min(HOST_LEASE_MS),
                started: mono,
            };
            let grant_bytes = grant::uuid_bytes(&claims.grant).unwrap_or([0; 16]);
            if let Some(key) = proof_key {
                let authority = Authority::new(
                    fiup::header(grant_bytes, self.world_bytes, self.rules.proof_host(), xuid),
                    key,
                    claims.exp,
                );
                if let Some(mut old) = snap.proofs.insert(
                    grant_bytes,
                    ProofGrant {
                        peer: xuid,
                        entry,
                        authority,
                    },
                ) {
                    old.authority.zero();
                }
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
        out
    }

    /// The decision for (xuid, endpoint, kind 0 game / 1 hook), in §6.2's order: a live lease,
    /// then the endpoint's FIUP proof of a live grant (proof is always required).
    pub fn allowed(&mut self, xuid: u64, endpoint: SocketAddr, kind: u8, mono: u64) -> bool {
        let endpoint = crate::stun::plain_endpoint(endpoint);
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
        proof.allowed(&lookup, xuid, kind, endpoint, mono, unix)
    }

    /// Whether `xuid` holds a live grant its endpoint can prove (a held OpenConnection waits).
    pub fn proof_expected(&mut self, xuid: u64, mono: u64) -> bool {
        if !self.host_live(mono) {
            return false;
        }
        let unix = (self.now_ms(mono) / 1000) as i64;
        self.snapshot.as_ref().is_some_and(|s| {
            s.proofs
                .values()
                .any(|p| p.peer == xuid && p.entry.live(unix, mono))
        })
    }

    /// Whether this world's verified snapshot holds a FIUP grant with these id bytes (the public
    /// socket routes a FIUP frame by its grant id, contract §5.2).
    pub fn holds_grant(&self, grant: &[u8; 16]) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|s| s.proofs.contains_key(grant))
    }

    /// Whether `endpoint` holds a fresh FIUP binding in this world (the public socket's routing
    /// of every other datagram; the per-packet decision stays [`Gate::allowed`]).
    pub fn binds(&self, endpoint: SocketAddr, mono: u64) -> bool {
        self.proof.binds(endpoint, mono)
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
