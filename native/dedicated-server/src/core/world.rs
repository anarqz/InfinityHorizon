// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! One world, game-agnostic part (contract §4.3, §4.5, §6): its normalized `world.open`, its
//! socket, its admission state and its `world.state`; the game side is a `GameWorld`.
//!
//! Step 1: every command validates its input and answers per the contract; admission keeps the
//! context checks, the 45 s host lease (renewed by `world.context` only) and the clear-on-failure
//! rule, but verifies no grant yet (no Ed25519, FIUP, legs or hints), so nobody is admitted:
//! fail closed, as before the first `world.context`.

use std::net::SocketAddr;

use serde_json::{Map, Value, json};

use super::{
    Profile, SockId,
    events::{self, EventQueue},
    fields::{Obj, parse_xuid},
    game::{GameCtx, GameWorld, Notice, Send},
    timers::{Timer, Timers},
};
use crate::control::proto::{self, CmdError, CmdResult};

/// The host lease (`M/WorldRelayAdmission.cs` `HostLease`).
pub const HOST_LEASE_MS: u64 = 45_000;
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
    pub discovery: bool,
    pub upnp: bool,
    pub stun_servers: Vec<String>,
    pub stun_interval_ms: u64,
}

impl Default for NetworkSpec {
    fn default() -> NetworkSpec {
        NetworkSpec {
            port: 5101,
            discovery: true,
            upnp: true,
            stun_servers: crate::nat::STUN_SERVERS
                .iter()
                .map(|s| s.to_string())
                .collect(),
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
        let vm = host.str("vmId")?;
        if !(1..=64).contains(&vm.len())
            || !vm
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(host.bad("vmId"));
        }
        // how grants name `official:<vmId>` is S34's (contract §6.3, §15 Q1): until then an
        // official world is refused; the private tests' build opens it to exercise the caps
        if !cfg!(feature = "test-hooks") {
            return Err(host.bad("kind").reason("grant_form_pending"));
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
            let stun_servers = if n.has("stunServers") {
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
                d.stun_servers
            };
            Some(NetworkSpec {
                port: n.opt_int("port", 1, 65_535, d.port as u64)? as u16,
                discovery: n.opt_bool("discovery", d.discovery)?,
                upnp: n.opt_bool("upnp", d.upnp)?,
                stun_servers,
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

#[derive(Debug, Clone, Default)]
pub struct Admission {
    /// A successful `world.context` since the last clear.
    pub has_context: bool,
    /// The host lease's end (trusted Unix ms); `None` before a context or after a clear.
    pub lease_until: Option<u64>,
    pub punches: usize,
    pub grants: usize,
    pub legs: usize,
    /// `ds.admission.lapsed` was sent for the current lease.
    pub lapse_noticed: bool,
}

/// What a world command needs besides the world (borrowed from the server).
pub struct Ctx<'a> {
    pub profile: Profile,
    pub keys_empty: bool,
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

pub struct World {
    pub id: String,
    pub spec: OpenSpec,
    pub game: Box<dyn GameWorld>,
    pub sock: Option<SockId>,
    pub port: u16,
    pub dual: bool,
    pub reserved_mb: u64,
    pub opened_at: u64,
    pub clock: TrustedClock,
    pub admission: Admission,
    pub open_reply: Map<String, Value>,
    pub last_state: Option<Value>,
    pub last_state_at: Option<u64>,
}

impl World {
    pub fn new(id: &str, spec: OpenSpec, game: Box<dyn GameWorld>, now: u64) -> World {
        World {
            id: id.to_string(),
            spec,
            game,
            sock: None,
            port: 0,
            dual: false,
            reserved_mb: 0,
            opened_at: now,
            clock: TrustedClock::default(),
            admission: Admission::default(),
            open_reply: Map::new(),
            last_state: None,
            last_state_at: None,
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
        match (self.admission.lease_until, self.clock.now(mono)) {
            (Some(until), Some(now)) => now < until,
            _ => false,
        }
    }

    /// Clears admission (fail closed): no snapshot, no lease, no legs.
    pub fn clear_admission(&mut self, ctx: &mut Ctx, why: &str) {
        let had = self.admission.has_context || self.admission.lease_until.is_some();
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
        let host = Obj::new(world.obj("host")?, "world.host");
        let host_xuid = host.str("xuid")?;
        if ctx.keys_empty {
            self.clear_admission(ctx, "no_keys");
            return Err(CmdError::code(proto::NO_KEYS));
        }
        let host_ok = match &self.spec.official {
            Some(vm) => host_xuid.strip_prefix("official:") == Some(vm.as_str()),
            None => parse_xuid(host_xuid).is_some_and(|x| x != 0 && x == self.spec.host_xuid),
        };
        if !host_ok || game != self.spec.game || id != self.id {
            self.clear_admission(ctx, "context");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("context"));
        }
        self.clock.observe(server_ms, ctx.now);
        let now = self.clock.now(ctx.now).unwrap_or(server_ms);
        let until = now + HOST_LEASE_MS;
        self.admission.has_context = true;
        self.admission.lease_until = Some(until);
        self.admission.lapse_noticed = false;
        let mut m = Map::new();
        m.insert("leaseUntilMs".into(), json!(until));
        Ok(m)
    }

    /// `world.admission` (punches only): a new snapshot that never renews the lease; more than
    /// 256 punches or no context yet clears admission (fail closed).
    pub fn admission(&mut self, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let server_ms = o.int("serverTimeMs", 0, JSON_INT_MAX)?;
        let punches = o.arr("punches")?;
        if !self.admission.has_context {
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
        self.clock.observe(server_ms, ctx.now);
        self.admission.punches = punches.len();
        self.admission.grants = 0;
        self.admission.legs = 0;
        let mut m = Map::new();
        m.insert("admitted".into(), json!(self.admission.grants));
        m.insert("legs".into(), json!(self.admission.legs));
        m.insert("proofGrants".into(), json!(0));
        Ok(m)
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

    pub fn kick(&mut self, o: Obj, ctx: &mut Ctx, out: &mut Vec<Send>) -> CmdResult {
        let xuid = o.xuid("xuid")?;
        let dropped = self.game.kick(xuid, ctx.now, out);
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

    /// NetworkFacts (private worlds). Step 1: discovery and UPnP are not wired yet, so the facts
    /// are the bound port and family only.
    pub fn network_facts(&self) -> Option<Value> {
        self.spec.network.as_ref()?;
        Some(json!({
            "worldId": self.id,
            "port": self.port,
            "nat": "unknown",
            "publicAddresses": [],
            "upnp": "off",
            "behindAnotherNat": false,
            "dualStack": self.dual,
            "punchHints": 0,
            "punchesSent": 0,
            "sendErrors": 0,
        }))
    }

    /// `WorldState` (contract §4.5): the core's fields plus the game's.
    pub fn state(&mut self, mono: u64) -> Value {
        let live = self.lease_live(mono);
        let game = self.game.state();
        let mut m = Map::new();
        m.insert("worldId".into(), json!(self.id));
        m.insert("running".into(), Value::Bool(true));
        for k in ["peers", "lobbyPlayers", "activity", "engines"] {
            m.insert(k.into(), game.get(k).cloned().unwrap_or(Value::Null));
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
                "leaseUntilMs": self.admission.lease_until,
                "grants": self.admission.grants,
                "legs": self.admission.legs,
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

    /// A datagram for this world, handed to its game.
    pub fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>) {
        self.game.receive(data, from, now, out);
    }
}
