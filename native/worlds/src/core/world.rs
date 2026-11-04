// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! One world, game-agnostic part (contract §4.3, §4.5, §6): its normalized `world.open`, its
//! socket, its admission state and its `world.state`; the game side is a `GameWorld`.
//!
//! Admission (core::admission) verifies the punches, keeps the lease and answers the engines;
//! the world owns the datagram order of §5.1 (relay legs, FIUP, STUN, then the game), the NAT
//! of a private world and the control events of its game.

use std::net::SocketAddr;

use serde_json::{Map, Value, json};

use super::{
    Profile, SockId,
    admission::{Rules, Shared},
    events::{self, EventQueue},
    fields::{Obj, parse_xuid},
    game::{GameCtx, GameEvent, GameWorld, Notice, Send},
    grant,
    keys::KeyRing,
    legs::{self, Legs, Received},
    timers::{Timer, Timers},
};
use crate::control::proto::{self, CmdError, CmdResult};
use crate::nat::NatProbe;

/// Punches per snapshot (contract §6.2: more is `context_invalid`).
pub const PUNCHES_MAX: usize = 256;
/// `world.state` at most 4 per second per world, at least every 15 s.
pub const STATE_MIN_GAP_MS: u64 = 250;
pub const STATE_REFRESH_MS: u64 = 15_000;
/// JSON numbers the controller reads exactly (2^53).
pub const JSON_INT_MAX: u64 = (1 << 53) - 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSpec {
    pub port: u16,
    /// `discovery`, `upnp` and `stunServers` of the contract's `network` object.
    pub resolve_stun: bool,
    pub map_port: bool,
    pub stun_names: Vec<String>,
    pub stun_interval_ms: u64,
}

impl Default for NetworkSpec {
    fn default() -> NetworkSpec {
        NetworkSpec {
            port: 5101,
            resolve_stun: true,
            map_port: true,
            stun_names: crate::nat::DEFAULT_STUN_NAMES.map(String::from).to_vec(),
            stun_interval_ms: 25_000,
        }
    }
}

/// The game-agnostic part of a normalized `world.open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSpec {
    pub game: String,
    /// A `player` host's XUID; 0 for an `official` host (no player XUID, contract §6.3).
    pub host_xuid: u64,
    /// An `official` host's VM id (`official:<vmId>`); `None` for a `player` host.
    pub official: Option<String>,
    pub is_player: bool,
    pub max_players: u8,
    pub require_proof: bool,
    pub dev_lan_peers: bool,
    pub network: Option<NetworkSpec>,
}

/// The world's game: `game` (OD-186); absent means `"fh2"`, the only game before the field.
pub fn game_of<'a>(o: Obj<'a>) -> Result<&'a str, CmdError> {
    match o.get("game") {
        None => Ok("fh2"),
        Some(Value::String(g)) if !g.is_empty() && g.len() <= 16 => Ok(g),
        Some(_) => Err(o.bad("game")),
    }
}

pub fn parse_open(o: Obj, profile: Profile) -> Result<OpenSpec, CmdError> {
    let game = game_of(o)?.to_string();
    // OD-186 item 7: a tagged host; private worlds take only `player`, public only `official`
    let host = Obj::new(o.obj("host")?, "host");
    match (host.str("kind")?, profile) {
        ("player", Profile::Private) => {}
        ("official", Profile::Public) => {}
        _ => return Err(host.bad("kind")),
    }
    let (host_xuid, official, is_player) = if profile == Profile::Public {
        // WORLDS_OFFICIAL.md §3: the `FI_WORLDS` entry's id, `[a-z0-9-]{1,32}`
        let vm = host.str("vmId")?;
        if !grant::vm_id(vm) {
            return Err(host.bad("vmId"));
        }
        (0, Some(vm.to_string()), false)
    } else {
        let xuid = host.xuid("xuid")?;
        if xuid == 0 {
            return Err(host.bad("xuid"));
        }
        (xuid, None, host.bool("isPlayer")?)
    };
    let max_players = o.int("maxPlayers", 1, 12)? as u8;
    // public worlds: proof forced (contract §4.3)
    let require_proof = o.bool("requireProof")? || profile == Profile::Public;
    let dev_lan_peers = o.opt_bool("devLanPeers", false)?;
    if dev_lan_peers && (require_proof || profile == Profile::Public) {
        return Err(o.bad("devLanPeers"));
    }
    let network = match (profile, o.has("network")) {
        (Profile::Public, true) => return Err(o.bad("network")),
        (Profile::Public, false) => None,
        (Profile::Private, false) => Some(NetworkSpec::default()),
        (Profile::Private, true) => {
            let n = Obj::new(o.obj("network")?, "network");
            let d = NetworkSpec::default();
            let stun_names = if n.has("stunServers") {
                let list = n.arr("stunServers")?;
                if list.len() > 8 {
                    return Err(n.bad("stunServers"));
                }
                let mut out = Vec::new();
                for v in list {
                    let s = v
                        .as_str()
                        .filter(|s| host_port(s))
                        .ok_or_else(|| n.bad("stunServers"))?;
                    out.push(s.to_string());
                }
                out
            } else {
                d.stun_names
            };
            Some(NetworkSpec {
                port: n.opt_int("port", 1, 65_535, d.port as u64)? as u16,
                resolve_stun: n.opt_bool("discovery", d.resolve_stun)?,
                map_port: n.opt_bool("upnp", d.map_port)?,
                stun_names,
                stun_interval_ms: n.opt_int("stunIntervalMs", 50, 3_600_000, d.stun_interval_ms)?,
            })
        }
    };
    Ok(OpenSpec {
        game,
        host_xuid,
        official,
        is_player,
        max_players,
        require_proof,
        dev_lan_peers,
        network,
    })
}

/// `host:port` with a non-empty host of at most 253 characters and a port 1-65535.
fn host_port(s: &str) -> bool {
    let Some((host, port)) = s.rsplit_once(':') else {
        return false;
    };
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'[' | b']' | b':'))
        && port.parse::<u16>().is_ok_and(|p| p > 0)
}

/// What the core keeps besides the gate: the lapse notice and the hints sent.
#[derive(Debug, Clone, Default)]
pub struct Admission {
    /// `ds.admission.lapsed` was sent for the current lease.
    pub lapse_noticed: bool,
}

/// What a world command needs besides the world (borrowed from the server).
pub struct Ctx<'a> {
    pub profile: Profile,
    pub keys_empty: bool,
    /// The relay-grant ring (`keys.set`).
    pub keys: &'a KeyRing,
    pub events: &'a mut EventQueue,
    pub timers: &'a mut Timers,
    pub now: u64,
}

impl Ctx<'_> {
    pub fn notice(&mut self, id: &str, level: &str, world: Option<&str>, args: Option<Value>) {
        let mut body = Map::new();
        body.insert("id".into(), Value::String(id.into()));
        body.insert("level".into(), Value::String(level.into()));
        if let Some(w) = world {
            body.insert("worldId".into(), Value::String(w.into()));
        }
        if let Some(a) = args {
            body.insert("args".into(), a);
        }
        self.events.push(events::NOTICE, world, body);
    }

    pub fn game_notices(&mut self, world: &str, notices: Vec<Notice>) {
        for n in notices {
            self.notice(n.id, n.level, Some(world), n.args);
        }
    }
}

/// The `diag` event: at most every 3 s, at most 2048 per world, and one at close.
const DIAG_EVERY_MS: u64 = 3000;
const DIAG_MAX: u32 = 2048;
/// `peer.rejected`: at most 128 per 30 s per world, one per (xuid, kind) per second.
const REJECTED_WINDOW_MS: u64 = 30_000;
const REJECTED_PER_WINDOW: u32 = 128;
const REJECTED_DEDUP_MS: u64 = 1000;
/// The network timers' step (STUN rounds, punches, UPnP polling, relay legs).
pub const NET_TICK_MS: u64 = 100;

pub struct World {
    pub id: String,
    pub spec: OpenSpec,
    pub game: Box<dyn GameWorld>,
    pub sock: Option<SockId>,
    pub port: u16,
    pub dual: bool,
    pub reserved_mb: u64,
    pub opened_at: u64,
    /// The admission the engines ask (grants, lease, FIUP).
    pub gate: Shared,
    pub admission: Admission,
    /// The relay legs (CR-S57).
    pub legs: Legs,
    /// STUN, UPnP and punches (private worlds).
    pub nat: Option<NatProbe>,
    pub punch_hints: u32,
    pub punches_sent: u32,
    pub send_errors: u32,
    pub open_reply: Map<String, Value>,
    pub last_state: Option<Value>,
    pub last_state_at: Option<u64>,
    last_network: Option<Value>,
    /// Since when nobody but the host's own game is in (`world.empty`); `None`: occupied.
    empty_since: Option<u64>,
    occupied_once: bool,
    diag_at: Option<u64>,
    diag_count: u32,
    rejected: (u64, u32),
    rejected_seen: Vec<(u64, &'static str, u64)>,
    /// Granted punches already sent per grant: (revision, window start, sent, last seen).
    granted_punches: Vec<([u8; 16], u32, u64, u32, u64)>,
    /// test-hooks `debug.fault {where: "after"}`: the next follow-up (events, state) panics.
    #[cfg(feature = "test-hooks")]
    pub fault_after: bool,
}

impl World {
    pub fn new(
        id: &str,
        spec: OpenSpec,
        game: Box<dyn GameWorld>,
        gate: Shared,
        now: u64,
    ) -> World {
        World {
            id: id.to_string(),
            spec,
            game,
            sock: None,
            port: 0,
            dual: false,
            reserved_mb: 0,
            opened_at: now,
            gate,
            admission: Admission::default(),
            legs: Legs::default(),
            nat: None,
            punch_hints: 0,
            punches_sent: 0,
            send_errors: 0,
            open_reply: Map::new(),
            last_state: None,
            last_state_at: None,
            last_network: None,
            empty_since: Some(now),
            occupied_once: false,
            diag_at: None,
            diag_count: 0,
            rejected: (now, 0),
            rejected_seen: Vec::new(),
            granted_punches: Vec::new(),
            #[cfg(feature = "test-hooks")]
            fault_after: false,
        }
    }

    /// The admission's rules for a world being opened.
    pub fn rules(id: &str, spec: &OpenSpec) -> Rules {
        Rules {
            world_id: id.to_string(),
            host: match &spec.official {
                Some(vm) => grant::official_host(vm),
                None => spec.host_xuid.to_string(),
            },
            host_xuid: spec.host_xuid,
            is_player: spec.is_player && spec.official.is_none(),
            require_proof: spec.require_proof,
            dev_lan_peers: spec.dev_lan_peers,
        }
    }

    pub fn synced(&self) -> usize {
        self.game.synced()
    }

    pub fn handshaking(&self) -> usize {
        self.game.handshaking()
    }

    /// Whether the host lease is live at `mono`.
    pub fn lease_live(&mut self, mono: u64) -> bool {
        self.gate.lock().host_live(mono)
    }

    pub fn lease_until(&self) -> Option<u64> {
        self.gate.lock().lease_until()
    }

    /// Clears admission (fail closed): no snapshot, no lease, no legs.
    pub fn clear_admission(&mut self, ctx: &mut Ctx, why: &str) {
        let had = {
            let mut g = self.gate.lock();
            let had = g.has_context || g.lease_until().is_some();
            g.clear();
            had
        };
        self.legs.clear();
        self.admission = Admission::default();
        if had {
            let id = self.id.clone();
            ctx.notice(
                events::N_ADMISSION_CLEARED,
                "warn",
                Some(&id),
                Some(json!({"why": why})),
            );
        }
    }

    /// `world.context` (the context only, never punches; contract §4.3): the checks of §6.2, the
    /// clock and the 45 s host lease (only this renews it). Any failure clears admission.
    pub fn context(&mut self, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let server_ms = o.int("serverTimeMs", 0, JSON_INT_MAX)?;
        let world = Obj::new(o.obj("world")?, "world");
        let id = world.str("id")?;
        let game = world.str("game")?;
        // the host as the grants name it (WORLDS_OFFICIAL.md §3): `{xuid}` for a player host
        // (unchanged), `{official: vmId}` alone for an official world
        let host = Obj::new(world.obj("host")?, "world.host");
        let official = match &self.spec.official {
            Some(_) => Some((host.str("official")?, host.map.len() == 1)),
            None => None,
        };
        let host_xuid = match official {
            Some(_) => "",
            None => host.str("xuid")?,
        };
        if ctx.keys_empty {
            self.clear_admission(ctx, "no_keys");
            return Err(CmdError::code(proto::NO_KEYS));
        }
        let host_ok = match (&self.spec.official, official) {
            (Some(vm), Some((named, alone))) => alone && named == vm.as_str(),
            _ => parse_xuid(host_xuid).is_some_and(|x| x != 0 && x == self.spec.host_xuid),
        };
        if !host_ok || game != self.spec.game || id != self.id || !grant::uuid(id) {
            self.clear_admission(ctx, "context");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("context"));
        }
        let until = self.gate.lock().context(server_ms, ctx.now);
        self.admission.lapse_noticed = false;
        let mut m = Map::new();
        m.insert("leaseUntilMs".into(), json!(until));
        Ok(m)
    }

    /// `world.admission` (punches only): a new verified snapshot that never renews the lease;
    /// more than 256 punches or no context yet clears admission (fail closed).
    pub fn admission(&mut self, o: Obj, ctx: &mut Ctx, out: &mut Vec<Send>) -> CmdResult {
        let server_ms = o.int("serverTimeMs", 0, JSON_INT_MAX)?;
        let punches = o.arr("punches")?;
        if !self.gate.lock().has_context {
            self.clear_admission(ctx, "no_context");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("no_context"));
        }
        if punches.len() > PUNCHES_MAX || punches.iter().any(|p| !p.is_object()) {
            self.clear_admission(ctx, "punches");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("punches"));
        }
        if ctx.keys_empty {
            self.clear_admission(ctx, "no_keys");
            return Err(CmdError::code(proto::NO_KEYS));
        }
        let outcome = self
            .gate
            .lock()
            .admission(punches, ctx.keys, server_ms, ctx.now);
        let mut sends = Vec::new();
        self.legs.update(outcome.legs, ctx.now, &mut sends);
        self.send_out(sends, out);
        // a kicked punch: its grant is revoked (above) and its XUID's peers go
        for x in &outcome.kicked {
            let mut sends = Vec::new();
            self.game.kick(*x, ctx.now, &mut sends);
            self.send_out(sends, out);
        }
        for (grant, revision, endpoints) in &outcome.hints {
            self.punch_granted(*grant, *revision, endpoints, ctx.now, out);
        }
        self.game.set_grant_voice_blocks(&outcome.voice_blocks);
        let mut m = Map::new();
        m.insert("admitted".into(), json!(outcome.admitted));
        m.insert("legs".into(), json!(self.legs.len()));
        m.insert("proofGrants".into(), json!(outcome.proof_grants));
        Ok(m)
    }

    /// One grant's candidates, punched with two Binding requests each, at most 10 per 15 s per
    /// grant, a new revision only (`RelayNetworkNat.PunchGranted`).
    fn punch_granted(
        &mut self,
        grant: [u8; 16],
        revision: u32,
        endpoints: &[SocketAddr],
        now: u64,
        out: &mut Vec<Send>,
    ) {
        if self.nat.is_none() || endpoints.len() > 5 {
            return;
        }
        self.granted_punches
            .retain(|p| now.saturating_sub(p.4) < 90_000);
        let previous = self.granted_punches.iter().position(|p| p.0 == grant);
        let (prev_rev, mut window, mut sent) = previous.map_or((0, 0, 0), |i| {
            let p = self.granted_punches[i];
            (p.1, p.2, p.3)
        });
        if revision <= prev_rev || (previous.is_none() && self.granted_punches.len() >= 64) {
            return;
        }
        if now.saturating_sub(window) >= 15_000 {
            window = now;
            sent = 0;
        }
        for e in endpoints {
            if sent + 2 > 10 {
                break;
            }
            if e.is_ipv6() && !self.dual {
                continue;
            }
            for _ in 0..2 {
                let t = crate::stun::random_txid();
                out.push(Send {
                    to: crate::stun::plain_endpoint(*e),
                    data: crate::stun::request_binding(&t).to_vec(),
                });
            }
            sent += 2;
            self.punches_sent = self.punches_sent.saturating_add(2);
        }
        self.punch_hints = self.punch_hints.saturating_add(1);
        let entry = (grant, revision, window, sent, now);
        match previous {
            Some(i) => self.granted_punches[i] = entry,
            None => self.granted_punches.push(entry),
        }
    }

    pub fn admission_end(&mut self, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let reason = o.str("reason")?;
        if !matches!(
            reason,
            "pause" | "unauthorized" | "not_found" | "not_host" | "wrong_pool"
        ) {
            return Err(o.bad("reason"));
        }
        self.clear_admission(ctx, reason);
        Ok(Map::new())
    }

    /// `world.kick`: every grant of the XUID revoked for 90 s, its FIUP keys zeroed, its peers
    /// dropped in every engine.
    pub fn kick(&mut self, o: Obj, ctx: &mut Ctx, out: &mut Vec<Send>) -> CmdResult {
        let xuid = o.xuid("xuid")?;
        self.gate.lock().revoke(xuid, ctx.now);
        let mut sends = Vec::new();
        let dropped = self.game.kick(xuid, ctx.now, &mut sends);
        self.send_out(sends, out);
        let mut m = Map::new();
        m.insert("dropped".into(), json!(dropped));
        Ok(m)
    }

    /// A game-scoped command, through the game module.
    pub fn game_command(&mut self, op: &str, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let mut gctx = GameCtx {
            profile: ctx.profile,
            now: ctx.now,
            notices: Vec::new(),
        };
        let r = self
            .game
            .command(op, o, &mut gctx)
            .unwrap_or_else(|| Err(CmdError::code(proto::UNKNOWN_OP)));
        let id = self.id.clone();
        ctx.game_notices(&id, gctx.notices);
        r
    }

    /// NetworkFacts (private worlds, contract §4.5).
    pub fn network_facts(&self) -> Option<Value> {
        self.spec.network.as_ref()?;
        let facts = self.nat.as_ref().map(NatProbe::exposure);
        let public: Vec<Value> = facts.as_ref().map_or_else(Vec::new, |f| {
            f.endpoints
                .iter()
                .map(|a| json!({"address": a.ip().to_string(), "port": a.port()}))
                .collect()
        });
        let mut m = Map::new();
        m.insert("worldId".into(), json!(self.id));
        m.insert("port".into(), json!(self.port));
        m.insert(
            "nat".into(),
            json!(facts.as_ref().map_or("unknown", |f| f.mapping.label())),
        );
        m.insert("publicAddresses".into(), Value::Array(public));
        m.insert(
            "upnp".into(),
            json!(facts.as_ref().map_or("off", |f| f.port_map.label())),
        );
        if let Some(a) = facts.as_ref().and_then(|f| f.mapped_endpoint) {
            m.insert(
                "upnpAddress".into(),
                json!({"address": a.ip().to_string(), "port": a.port()}),
            );
        }
        m.insert(
            "behindAnotherNat".into(),
            json!(facts.as_ref().is_some_and(|f| f.double_nat)),
        );
        m.insert("dualStack".into(), json!(self.dual));
        m.insert("punchHints".into(), json!(self.punch_hints));
        m.insert("punchesSent".into(), json!(self.punches_sent));
        m.insert("sendErrors".into(), json!(self.send_errors));
        Some(Value::Object(m))
    }

    /// `WorldState` (contract §4.5): the core's fields plus the game's.
    pub fn state(&mut self, mono: u64) -> Value {
        let live = self.lease_live(mono);
        let (until, grants) = {
            let g = self.gate.lock();
            (g.lease_until(), g.grants())
        };
        let game = self.game.state();
        let mut m = Map::new();
        m.insert("worldId".into(), json!(self.id));
        m.insert("running".into(), Value::Bool(true));
        for k in ["peers", "lobbyPlayers", "activity", "engines"] {
            m.insert(k.into(), game.get(k).cloned().unwrap_or(Value::Null));
        }
        // OD-194: present (true) only while the activity takes no newcomers
        if let Some(v) = game.get("inProgress").filter(|v| v.as_bool() == Some(true)) {
            m.insert("inProgress".into(), v.clone());
        }
        m.insert("handshaking".into(), json!(self.handshaking()));
        m.insert(
            "hostInWorld".into(),
            game.get("hostInWorld")
                .cloned()
                .unwrap_or(Value::Bool(false)),
        );
        m.insert(
            "admission".into(),
            json!({
                "live": live,
                "leaseUntilMs": until,
                "grants": grants,
                "legs": self.legs.len(),
            }),
        );
        if let Some(n) = self.network_facts() {
            m.insert("network".into(), n);
        }
        Value::Object(m)
    }

    /// Emits `world.state` when it changed (at most every 250 ms: later changes are flushed by a
    /// timer) or when `refresh` and 15 s passed since the last one.
    pub fn emit_state(&mut self, ctx: &mut Ctx, refresh: bool) {
        let value = self.state(ctx.now);
        let changed = self.last_state.as_ref() != Some(&value);
        let since = self.last_state_at.map(|t| ctx.now.saturating_sub(t));
        let due_refresh = refresh && since.is_none_or(|s| s >= STATE_REFRESH_MS);
        if !changed && !due_refresh {
            return;
        }
        if changed
            && !due_refresh
            && let Some(s) = since
            && s < STATE_MIN_GAP_MS
        {
            let at = self.last_state_at.unwrap_or(ctx.now) + STATE_MIN_GAP_MS;
            ctx.timers.schedule(Timer::StateFlush(self.id.clone()), at);
            return;
        }
        if let Value::Object(body) = value.clone() {
            ctx.events.push(events::WORLD_STATE, Some(&self.id), body);
        }
        self.last_state = Some(value);
        self.last_state_at = Some(ctx.now);
    }

    /// The game's events (peer.synced, peer.left, peer.rejected), the empty/occupied hints and
    /// `world.network`, after anything the world did.
    pub fn emit_events(&mut self, ctx: &mut Ctx) {
        #[cfg(feature = "test-hooks")]
        if self.fault_after {
            panic!("debug.fault after");
        }
        for e in self.game.take_events() {
            match e {
                GameEvent::PeerSynced { xuid, engine, peer } => {
                    let body = obj(json!({"worldId": self.id, "xuid": xuid.to_string(),
                        "engine": engine, "peer": peer}));
                    ctx.events.push(events::PEER_SYNCED, Some(&self.id), body);
                }
                GameEvent::PeerLeft {
                    xuid,
                    engine,
                    peer,
                    reason,
                } => {
                    let body = obj(json!({"worldId": self.id, "xuid": xuid.to_string(),
                        "engine": engine, "peer": peer, "reason": reason}));
                    ctx.events.push(events::PEER_LEFT, Some(&self.id), body);
                }
                GameEvent::Rejected {
                    xuid,
                    peer,
                    kind,
                    message,
                    router,
                    value,
                } => self.rejected(ctx, (xuid, peer), kind, message, router, value),
                GameEvent::Custom {
                    ev,
                    world,
                    mut body,
                } => {
                    if world {
                        body.insert("worldId".into(), json!(self.id));
                    }
                    let scope = world.then_some(self.id.as_str());
                    ctx.events.push(ev, scope, body);
                }
            }
        }
        // hints: nobody but the host's own game, synced or handshaking
        let host = if self.spec.is_player {
            self.spec.host_xuid
        } else {
            0
        };
        let others = self.game.occupants(host);
        match (others == 0, self.empty_since) {
            (true, None) => {
                self.empty_since = Some(ctx.now);
                let since = self.gate.lock().clock.now(ctx.now).unwrap_or(ctx.now);
                let body = obj(json!({"worldId": self.id, "sinceMs": since}));
                ctx.events.push(events::WORLD_EMPTY, Some(&self.id), body);
            }
            (false, Some(_)) => {
                self.empty_since = None;
                self.occupied_once = true;
                let body = obj(json!({"worldId": self.id}));
                ctx.events
                    .push(events::WORLD_OCCUPIED, Some(&self.id), body);
            }
            _ => {}
        }
        if let Some(n) = self.network_facts()
            && self.last_network.as_ref() != Some(&n)
        {
            if let Value::Object(body) = n.clone() {
                ctx.events.push(events::WORLD_NETWORK, Some(&self.id), body);
            }
            self.last_network = Some(n);
        }
    }

    fn rejected(
        &mut self,
        ctx: &mut Ctx,
        (xuid, peer): (u64, Option<u8>),
        kind: &'static str,
        message: Option<u32>,
        router: Option<u32>,
        value: Option<u32>,
    ) {
        let now = ctx.now;
        self.rejected_seen
            .retain(|r| now.saturating_sub(r.2) < REJECTED_DEDUP_MS);
        if self
            .rejected_seen
            .iter()
            .any(|r| r.0 == xuid && r.1 == kind)
        {
            return;
        }
        if now.saturating_sub(self.rejected.0) >= REJECTED_WINDOW_MS {
            self.rejected = (now, 0);
        }
        if self.rejected.1 >= REJECTED_PER_WINDOW {
            return;
        }
        self.rejected.1 += 1;
        self.rejected_seen.push((xuid, kind, now));
        let mut body = obj(json!({"worldId": self.id, "xuid": xuid.to_string(), "kind": kind}));
        for (k, v) in [("message", message), ("router", router), ("value", value)] {
            if let Some(v) = v {
                body.insert(k.into(), json!(v));
            }
        }
        ctx.events.push(events::PEER_REJECTED, Some(&self.id), body);
        // stdout names the peer by its engine index only (§2.3)
        let mut line = crate::log::info(crate::log_ids::PEER_REJECTED).world(&self.id);
        if let Some(p) = peer {
            line = line.field("peer", p);
        }
        line.field("kind", kind).emit();
    }

    /// The `diag` event (at most every 3 s, 2048 per world; `final_one` at close).
    pub fn diag(&mut self, ctx: &mut Ctx, final_one: bool) {
        if !final_one
            && (self.diag_count >= DIAG_MAX
                || self
                    .diag_at
                    .is_some_and(|t| ctx.now.saturating_sub(t) < DIAG_EVERY_MS))
        {
            return;
        }
        self.diag_at = Some(ctx.now);
        self.diag_count += 1;
        let c = self.gate.lock().fiup_counters();
        let facts = self.nat.as_ref().map(NatProbe::exposure);
        let body = obj(json!({
            "worldId": self.id,
            "fiup": {
                "helloGame": c.hello_game, "helloHook": c.hello_hook,
                "responseGame": c.response_game, "responseHook": c.response_hook,
                "challengeGame": c.challenge_game, "challengeHook": c.challenge_hook,
                "ackGame": c.ack_game, "ackHook": c.ack_hook, "rejected": c.rejected,
            },
            "nat": facts.as_ref().map_or("unknown", |f| f.mapping.label()),
            "observations": facts.as_ref().map_or(0, |f| f.endpoints.len()),
            "upnp": facts.as_ref().map_or("off", |f| f.port_map.label()),
            "punchHints": self.punch_hints,
            "punchesSent": self.punches_sent,
            "sendErrors": self.send_errors,
        }));
        ctx.events.push(events::DIAG, Some(&self.id), body);
    }

    /// The world's datagrams out: to a relayed joiner as FIR1 `data` to its relay; never to
    /// another virtual address (a leg not confirmed); the rest as they are.
    pub fn send_out(&self, sends: Vec<Send>, out: &mut Vec<Send>) {
        for s in sends {
            if let Some(w) = self.legs.wrap(s.to, &s.data) {
                out.push(w);
            } else if !legs::is_virtual(s.to) {
                out.push(s);
            }
        }
    }

    /// A datagram for this world, in the contract's order (§5.1): FIR1 from a leg's relay (its
    /// `data` becomes the virtual peer's datagram), FIUP, STUN for the NAT (non-virtual), then
    /// the game. A public world has no legs and no NAT: FIUP, then the game (§5.2).
    pub fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>) {
        let mut sends = Vec::new();
        let (from, payload) = match self.legs.receive(data, from, now, &mut sends) {
            Received::Handled => {
                self.send_out(sends, out);
                return;
            }
            Received::Data(peer, payload) => (peer, payload),
            // public: no legs, every datagram comes straight from the joiner (§5.2)
            Received::NotOurs => (from, data.to_vec()),
        };
        self.send_out(sends, out);
        if payload.len() >= 4 && &payload[..4] == b"FIUP" {
            let reply = self.gate.lock().receive_proof(&payload, from, now);
            if let Some(r) = reply {
                self.send_out(vec![Send { to: from, data: r }], out);
            }
            return;
        }
        if !legs::is_virtual(from)
            && let Some(nat) = self.nat.as_mut()
        {
            let mut stun_out = Vec::new();
            nat.handle_datagram(&payload, from, now, &mut |to, data| {
                stun_out.push(Send { to, data })
            });
            out.extend(stun_out);
        }
        let mut game_out = Vec::new();
        self.game.receive(&payload, from, now, &mut game_out);
        self.send_out(game_out, out);
    }

    /// The network timers: relay legs, STUN rounds, punches, UPnP; `diag`.
    pub fn net_tick(&mut self, ctx: &mut Ctx, out: &mut Vec<Send>) {
        let now = ctx.now;
        let mut sends = Vec::new();
        self.legs.tick(now, &mut sends);
        out.extend(sends);
        if let Some(nat) = self.nat.as_mut() {
            let results = nat.service(now, &mut |to, data| out.push(Send { to, data }));
            for result in results {
                let (id, level) = match result {
                    crate::upnp::MapResult::Mapped { .. } => (events::N_UPNP_MAPPED, "info"),
                    _ => (events::N_UPNP_FAILED, "warn"),
                };
                let wid = self.id.clone();
                ctx.notice(id, level, Some(&wid), None);
            }
        }
        self.diag(ctx, false);
    }

    /// The open reply carried the first NetworkFacts: `world.network` follows only changes.
    pub fn mark_network(&mut self) {
        self.last_network = self.network_facts();
    }

    /// When the network timers next have work: relay legs (while any exists), STUN rounds and
    /// punches, a lookup or UPnP mapping to poll; `None`: nothing (no timer).
    pub fn net_deadline(&self, now: u64) -> Option<u64> {
        let legs = self.legs.next_deadline(now);
        let nat = self.nat.as_ref().and_then(|n| {
            let poll = n.awaiting_thread().then_some(now + NET_TICK_MS);
            match (n.wake_at(), poll) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        });
        match (legs, nat) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(|t| t.max(now))
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
