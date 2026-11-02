// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! One world (contract §4.3, §4.5, §6): its normalized `world.open` request, its engines, its
//! admission state, its voice and watch settings, and its `world.state`.
//!
//! Step 1: every command validates its input and answers per the contract; admission keeps the
//! context checks, the 45 s host lease (renewed by `world.context` only) and the clear-on-failure
//! rule, but verifies no grant yet (no Ed25519, FIUP, legs or hints), so nobody is admitted:
//! fail closed, as before the first `world.context`.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::{
    Catalog, Profile, SockId,
    events::{self, EventQueue},
    fields::{Obj, parse_xuid},
    timers::{Timer, Timers},
};
use crate::{
    control::proto::{self, CmdError, CmdResult},
    engine::{Engine, EngineKind, RelaySettings, Send},
};

/// The host lease (`M/WorldRelayAdmission.cs` `HostLease`).
pub const HOST_LEASE_MS: u64 = 45_000;
/// Punches per snapshot (contract §6.2: more is `context_invalid`).
pub const PUNCHES_MAX: usize = 256;
/// `world.voice.lobbyMuted`, `world.voiceBlocks.blocks`.
pub const VOICE_LIST_MAX: usize = 64;
/// `world.state` at most 4 per second per world, at least every 15 s.
pub const STATE_MIN_GAP_MS: u64 = 250;
pub const STATE_REFRESH_MS: u64 = 15_000;
/// JSON numbers the controller reads exactly (2^53).
pub const JSON_INT_MAX: u64 = (1 << 53) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMode {
    Off,
    Open,
    Proximity,
}

impl VoiceMode {
    pub fn parse(s: &str) -> Option<VoiceMode> {
        match s {
            "off" => Some(VoiceMode::Off),
            "open" => Some(VoiceMode::Open),
            "proximity" => Some(VoiceMode::Proximity),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voice {
    pub mode: VoiceMode,
    pub lobby_muted: Vec<u64>,
}

impl Default for Voice {
    fn default() -> Voice {
        Voice {
            mode: VoiceMode::Open,
            lobby_muted: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    /// MATCHMAKING_API §3.9 `WatchRules`, verbatim (`None`: stopped).
    pub rules: Option<Value>,
    pub self_xuid: Option<u64>,
    pub parts: bool,
    pub force_speed: bool,
}

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

/// A normalized `world.open` (the same values answer `ok` again, idempotent; others `exists`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSpec {
    pub host_xuid: u64,
    pub is_player: bool,
    pub max_players: u8,
    pub races: bool,
    pub require_proof: bool,
    pub dev_lan_peers: bool,
    pub voice: Voice,
    pub watch: Option<Watch>,
    pub network: Option<NetworkSpec>,
    pub settings: RelaySettings,
}

fn xuid_list(o: Obj, key: &str, max: usize) -> Result<Vec<u64>, CmdError> {
    let mut out = Vec::new();
    for v in o.arr(key)? {
        let x = v.as_str().and_then(parse_xuid).ok_or_else(|| o.bad(key))?;
        out.push(x);
    }
    if out.len() > max {
        return Err(o.bad(key));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

pub fn parse_voice(o: Obj) -> Result<Voice, CmdError> {
    let mode = VoiceMode::parse(o.str("mode")?).ok_or_else(|| o.bad("mode"))?;
    let lobby_muted = if o.has("lobbyMuted") {
        xuid_list(o, "lobbyMuted", VOICE_LIST_MAX)?
    } else {
        Vec::new()
    };
    Ok(Voice { mode, lobby_muted })
}

pub fn parse_watch(o: Obj, profile: Profile, is_player: bool) -> Result<Watch, CmdError> {
    let rules = match o.map.get("rules") {
        Some(Value::Null) => None,
        Some(v @ Value::Object(_)) => Some(v.clone()),
        _ => return Err(o.bad("rules")),
    };
    let self_xuid = if o.has("self") {
        if !is_player {
            return Err(o.bad("self"));
        }
        Some(o.xuid("self")?)
    } else {
        None
    };
    let parts = o.bool("parts")?;
    let force_speed = o.opt_bool("forceSpeed", false)?;
    if force_speed && profile == Profile::Public {
        return Err(o.bad("forceSpeed"));
    }
    Ok(Watch {
        rules,
        self_xuid,
        parts,
        force_speed,
    })
}

pub fn parse_open(o: Obj, profile: Profile) -> Result<OpenSpec, CmdError> {
    let host = Obj::new(o.obj("host")?, "host");
    let host_xuid = host.xuid("xuid")?;
    if host_xuid == 0 {
        return Err(host.bad("xuid"));
    }
    let is_player = host.bool("isPlayer")?;
    if is_player && profile == Profile::Public {
        return Err(host.bad("isPlayer"));
    }
    let max_players = o.int("maxPlayers", 1, 12)? as u8;
    let races = o.bool("races")?;
    // public worlds: proof forced (contract §4.3)
    let require_proof = o.bool("requireProof")? || profile == Profile::Public;
    let dev_lan_peers = o.opt_bool("devLanPeers", false)?;
    if dev_lan_peers && (require_proof || profile == Profile::Public) {
        return Err(o.bad("devLanPeers"));
    }
    let voice = if o.has("voice") {
        parse_voice(Obj::new(o.obj("voice")?, "voice"))?
    } else {
        Voice::default()
    };
    let watch = if o.has("watch") {
        Some(parse_watch(
            Obj::new(o.obj("watch")?, "watch"),
            profile,
            is_player,
        )?)
    } else {
        None
    };
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
    let mut settings = RelaySettings::for_profile(profile);
    if o.has("settings") {
        settings.apply(o.obj("settings")?)?;
    }
    Ok(OpenSpec {
        host_xuid,
        is_player,
        max_players,
        races,
        require_proof,
        dev_lan_peers,
        voice,
        watch,
        network,
        settings,
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
    pub catalog: Catalog,
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
}

pub struct World {
    pub id: String,
    pub spec: OpenSpec,
    pub engines: BTreeMap<EngineKind, Engine>,
    pub sock: Option<SockId>,
    pub port: u16,
    pub dual: bool,
    pub reserved_mb: u64,
    pub opened_at: u64,
    pub lobbies: Vec<&'static str>,
    pub clock: TrustedClock,
    pub admission: Admission,
    pub roster_blocks: BTreeMap<u64, u64>,
    pub open_reply: Map<String, Value>,
    pub last_state: Option<Value>,
    pub last_state_at: Option<u64>,
    /// Datagrams dropped by this world (step 1: all).
    pub dropped: u64,
}

impl World {
    pub fn new(id: &str, spec: OpenSpec, engines: BTreeMap<EngineKind, Engine>, now: u64) -> World {
        World {
            id: id.to_string(),
            spec,
            engines,
            sock: None,
            port: 0,
            dual: false,
            reserved_mb: 0,
            opened_at: now,
            lobbies: vec!["carmeet"],
            clock: TrustedClock::default(),
            admission: Admission::default(),
            roster_blocks: BTreeMap::new(),
            open_reply: Map::new(),
            last_state: None,
            last_state_at: None,
            dropped: 0,
        }
    }

    pub fn synced(&self) -> usize {
        self.engines.values().map(Engine::synced).sum()
    }

    pub fn handshaking(&self) -> usize {
        self.engines.values().map(Engine::handshaking).sum()
    }

    pub fn idle(&self) -> bool {
        self.engines.values().all(Engine::is_idle)
    }

    /// The trusted clock's now, if a context gave one.
    pub fn trusted_now(&mut self, mono: u64) -> Option<u64> {
        self.clock.now(mono)
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
            ctx.notice(
                events::N_ADMISSION_CLEARED,
                "warn",
                Some(&self.id.clone()),
                Some(json!({"why": why})),
            );
        }
    }

    /// The common snapshot checks of `world.context` and `world.admission`.
    fn snapshot(&mut self, o: Obj, ctx: &mut Ctx) -> Result<(u64, usize), CmdError> {
        let server_ms = o.int("serverTimeMs", 0, JSON_INT_MAX)?;
        let punches = o.arr("punches")?;
        if punches.len() > PUNCHES_MAX || punches.iter().any(|p| !p.is_object()) {
            self.clear_admission(ctx, "punches");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("punches"));
        }
        if ctx.keys_empty {
            self.clear_admission(ctx, "no_keys");
            return Err(CmdError::code(proto::NO_KEYS));
        }
        Ok((server_ms, punches.len()))
    }

    fn snapshot_reply(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("admitted".into(), json!(self.admission.grants));
        m.insert("legs".into(), json!(self.admission.legs));
        m.insert("proofGrants".into(), json!(0));
        m
    }

    /// `world.context`: the checks of §6.2, the lease renewal, the snapshot (verification not
    /// ported: every punch is skipped).
    pub fn context(&mut self, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let world = Obj::new(o.obj("world")?, "world");
        let id = world.str("id")?;
        let game = world.str("game")?;
        let host = Obj::new(world.obj("host")?, "world.host");
        let host_xuid = host.str("xuid")?;
        let (server_ms, punches) = self.snapshot(o, ctx)?;
        let host_ok = parse_xuid(host_xuid).is_some_and(|x| x != 0 && x == self.spec.host_xuid);
        if !host_ok || game != "fh2" || id != self.id {
            self.clear_admission(ctx, "context");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("context"));
        }
        self.clock.observe(server_ms, ctx.now);
        let now = self.clock.now(ctx.now).unwrap_or(server_ms);
        self.admission.has_context = true;
        self.admission.lease_until = Some(now + HOST_LEASE_MS);
        self.admission.lapse_noticed = false;
        self.admission.punches = punches;
        self.admission.grants = 0;
        self.admission.legs = 0;
        Ok(self.snapshot_reply())
    }

    /// `world.admission`: a new snapshot without renewing the lease.
    pub fn admission(&mut self, o: Obj, ctx: &mut Ctx) -> CmdResult {
        if !self.admission.has_context {
            // validate the shape first so a malformed request stays bad_request
            o.int("serverTimeMs", 0, JSON_INT_MAX)?;
            o.arr("punches")?;
            self.clear_admission(ctx, "no_context");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("no_context"));
        }
        let (server_ms, punches) = self.snapshot(o, ctx)?;
        self.clock.observe(server_ms, ctx.now);
        self.admission.punches = punches;
        self.admission.grants = 0;
        self.admission.legs = 0;
        Ok(self.snapshot_reply())
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

    pub fn voice(&mut self, o: Obj) -> CmdResult {
        self.spec.voice = parse_voice(o)?;
        Ok(Map::new())
    }

    pub fn voice_blocks(&mut self, o: Obj) -> CmdResult {
        if o.str("source")? != "roster" {
            return Err(o.bad("source"));
        }
        let blocks = o.obj("blocks")?;
        if blocks.len() > VOICE_LIST_MAX {
            return Err(o.bad("blocks"));
        }
        let mut next = BTreeMap::new();
        for (k, v) in blocks {
            let x = parse_xuid(k).ok_or_else(|| o.bad("blocks"))?;
            let until = v
                .as_u64()
                .filter(|n| *n <= JSON_INT_MAX)
                .ok_or_else(|| o.bad("blocks"))?;
            next.insert(x, until);
        }
        let changed = next != self.roster_blocks;
        self.roster_blocks = next;
        let mut m = Map::new();
        m.insert("changed".into(), Value::Bool(changed));
        Ok(m)
    }

    pub fn watch(&mut self, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let w = parse_watch(o, ctx.profile, self.spec.is_player)?;
        if w.parts && w.rules.is_some() && !ctx.catalog.parts {
            let id = self.id.clone();
            ctx.notice(events::N_NO_PARTS, "info", Some(&id), None);
        }
        self.spec.watch = w.rules.is_some().then_some(w);
        Ok(Map::new())
    }

    pub fn kick(&mut self, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let xuid = o.xuid("xuid")?;
        let mut out: Vec<Send> = Vec::new();
        let dropped: usize = self
            .engines
            .values_mut()
            .map(|e| e.kick(xuid, ctx.now, &mut out))
            .sum();
        let mut m = Map::new();
        m.insert("dropped".into(), json!(dropped));
        Ok(m)
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

    /// `WorldState` (contract §4.5).
    pub fn state(&mut self, mono: u64) -> Value {
        let mut engines = Map::new();
        for (k, e) in &self.engines {
            engines.insert(k.as_str().into(), json!(e.synced()));
        }
        let live = self.lease_live(mono);
        let mut m = Map::new();
        m.insert("worldId".into(), json!(self.id));
        m.insert("running".into(), Value::Bool(true));
        m.insert("peers".into(), json!([]));
        m.insert(
            "lobbyPlayers".into(),
            json!(
                self.engines
                    .iter()
                    .filter(|(k, _)| **k != EngineKind::FreeRoam)
                    .map(|(_, e)| e.synced())
                    .sum::<usize>()
            ),
        );
        m.insert("activity".into(), Value::Null);
        m.insert("engines".into(), Value::Object(engines));
        m.insert("handshaking".into(), json!(self.handshaking()));
        m.insert("hostInWorld".into(), Value::Bool(false));
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

    /// A datagram for this world. Step 1: nobody is admitted; the free roam engine drops it.
    pub fn receive(
        &mut self,
        data: &[u8],
        from: std::net::SocketAddr,
        now: u64,
        out: &mut Vec<Send>,
    ) {
        if crate::stun::is_stun(data) && u16::from_be_bytes([data[0], data[1]]) == LIVERY_KIND {
            // the retired livery channel (contract §5.3)
            self.dropped += 1;
            return;
        }
        match self.engines.get_mut(&EngineKind::FreeRoam) {
            Some(e) => e.receive(data, from, now, out),
            None => self.dropped += 1,
        }
    }

    /// The world's engine ticks (only engines that are not idle).
    pub fn tick(&mut self, now: u64, out: &mut Vec<Send>) {
        for e in self.engines.values_mut().filter(|e| !e.is_idle()) {
            e.tick(now, out);
        }
    }

    /// The next engine tick: 20 ms with synced peers, 100 ms with only handshaking ones (public),
    /// none when idle (contract §8 "Ticks").
    pub fn next_tick(&self, profile: Profile, now: u64) -> Option<u64> {
        if self.idle() {
            None
        } else if self.synced() > 0 || profile == Profile::Private {
            Some(now + 20)
        } else {
            Some(now + 100)
        }
    }
}

/// The retired livery side channel's STUN message type (contract §5.3: dropped).
pub const LIVERY_KIND: u16 = 0x3E92;
