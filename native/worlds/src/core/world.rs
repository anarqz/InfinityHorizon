// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! One official world, game-agnostic part (contract §4.3, §4.5, §6): its normalized
//! `world.open`, its admission state and its `world.state`; the game side is a `GameWorld`.
//!
//! Admission (core::admission) verifies the punches, keeps the lease and answers the engines;
//! the world owns the datagram order of §5.2 (FIUP, then the game) and the control events of its
//! game.

use std::net::SocketAddr;

use serde_json::{Map, Value, json};

use super::{
    admission::{Rules, Shared},
    events::{self, EventQueue},
    fields::Obj,
    game::{GameCtx, GameEvent, GameWorld, Notice, Send},
    grant,
    keys::KeyRing,
    timers::{Timer, Timers},
};
use crate::control::proto::{self, CmdError, CmdResult};

/// Punches per snapshot (contract §6.2: more is `context_invalid`).
pub const PUNCHES_MAX: usize = 256;
/// `world.state` at most 4 per second per world, at least every 15 s.
pub const STATE_MIN_GAP_MS: u64 = 250;
pub const STATE_REFRESH_MS: u64 = 15_000;
/// JSON numbers the controller reads exactly (2^53).
pub const JSON_INT_MAX: u64 = (1 << 53) - 1;

/// The game-agnostic part of a normalized `world.open`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSpec {
    pub game: String,
    /// The official host's VM id (`official:<vmId>`, contract §6.3).
    pub vm_id: String,
    pub max_players: u8,
}

/// The world's game: `game` (OD-186); absent means `"fh2"`, the only game before the field.
pub fn game_of<'a>(o: Obj<'a>) -> Result<&'a str, CmdError> {
    match o.get("game") {
        None => Ok("fh2"),
        Some(Value::String(g)) if !g.is_empty() && g.len() <= 16 => Ok(g),
        Some(_) => Err(o.bad("game")),
    }
}

/// `maxPlayers`' ceiling for a game that names none, and for an unknown game (FH2's 12).
pub const MAX_PLAYERS_DEFAULT: u8 = 12;

/// `max_players`: the named game's ceiling of `maxPlayers` ([`super::game::Game::max_players`]).
/// OD-186 item 7: the host is `{kind: "official", vmId}` (WORLDS_OFFICIAL.md §3); proof is
/// always required (contract §4.3), so `requireProof` is read and forced, `devLanPeers` may only
/// be false, and there is no `network` object (no world socket of its own).
pub fn parse_open(o: Obj, max_players: u8) -> Result<OpenSpec, CmdError> {
    let game = game_of(o)?.to_string();
    let host = Obj::new(o.obj("host")?, "host");
    if host.str("kind")? != "official" {
        return Err(host.bad("kind"));
    }
    // WORLDS_OFFICIAL.md §3: the `FI_WORLDS` entry's id, `[a-z0-9-]{1,32}`
    let vm = host.str("vmId")?;
    if !grant::vm_id(vm) {
        return Err(host.bad("vmId"));
    }
    let max_players = o.int("maxPlayers", 1, u64::from(max_players))? as u8;
    o.bool("requireProof")?;
    if o.opt_bool("devLanPeers", false)? {
        return Err(o.bad("devLanPeers"));
    }
    if o.has("network") {
        return Err(o.bad("network"));
    }
    Ok(OpenSpec {
        game,
        vm_id: vm.to_string(),
        max_players,
    })
}

/// What the core keeps besides the gate: the lapse notice.
#[derive(Debug, Clone, Default)]
pub struct Admission {
    /// `ds.admission.lapsed` was sent for the current lease.
    pub lapse_noticed: bool,
}

/// What a world command needs besides the world (borrowed from the server).
pub struct Ctx<'a> {
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

pub struct World {
    pub id: String,
    pub spec: OpenSpec,
    pub game: Box<dyn GameWorld>,
    pub reserved_mb: u64,
    pub opened_at: u64,
    /// The admission the engines ask (grants, lease, FIUP).
    pub gate: Shared,
    pub admission: Admission,
    pub open_reply: Map<String, Value>,
    pub last_state: Option<Value>,
    pub last_state_at: Option<u64>,
    /// Since when nobody is in (`world.empty`); `None`: occupied.
    empty_since: Option<u64>,
    occupied_once: bool,
    diag_at: Option<u64>,
    diag_count: u32,
    rejected: (u64, u32),
    rejected_seen: Vec<(u64, &'static str, u64)>,
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
            reserved_mb: 0,
            opened_at: now,
            gate,
            admission: Admission::default(),
            open_reply: Map::new(),
            last_state: None,
            last_state_at: None,
            empty_since: Some(now),
            occupied_once: false,
            diag_at: None,
            diag_count: 0,
            rejected: (now, 0),
            rejected_seen: Vec::new(),
            #[cfg(feature = "test-hooks")]
            fault_after: false,
        }
    }

    /// The admission's rules for a world being opened.
    pub fn rules(id: &str, spec: &OpenSpec) -> Rules {
        Rules {
            world_id: id.to_string(),
            vm_id: spec.vm_id.clone(),
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

    /// Clears admission (fail closed): no snapshot, no lease.
    pub fn clear_admission(&mut self, ctx: &mut Ctx, why: &str) {
        let had = {
            let mut g = self.gate.lock();
            let had = g.has_context || g.lease_until().is_some();
            g.clear();
            had
        };
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
    /// clock and the 45 s host lease (only this renews it). Any failure clears admission. A
    /// private official world's first `private: true` makes its game private (`out`: what that
    /// sends to the players already in).
    pub fn context(&mut self, o: Obj, ctx: &mut Ctx, out: &mut Vec<Send>) -> CmdResult {
        let server_ms = o.int("serverTimeMs", 0, JSON_INT_MAX)?;
        let world = Obj::new(o.obj("world")?, "world");
        let id = world.str("id")?;
        let game = world.str("game")?;
        // the host as the grants name it (WORLDS_OFFICIAL.md §3): `{official: vmId}` alone
        let host = Obj::new(world.obj("host")?, "world.host");
        let named = host.str("official")?;
        let alone = host.map.len() == 1;
        // WORLDS_OFFICIAL.md §12.2 item 5, §12.8b (T-L9): `private: true` from the server's
        // `WorldView.private` (free roam or Road Trip); one way, so `false` or absent never
        // makes a private world public again
        let private = world.opt_bool("private", false)?;
        if private && !self.game.supports_private() {
            return Err(world.bad("private"));
        }
        if ctx.keys_empty {
            self.clear_admission(ctx, "no_keys");
            return Err(CmdError::code(proto::NO_KEYS));
        }
        let host_ok = alone && named == self.spec.vm_id;
        if !host_ok || game != self.spec.game || id != self.id || !grant::uuid(id) {
            self.clear_admission(ctx, "context");
            return Err(CmdError::code(proto::CONTEXT_INVALID).reason("context"));
        }
        let until = self.gate.lock().context(server_ms, ctx.now);
        self.admission.lapse_noticed = false;
        if private {
            self.game.set_private(out);
        }
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
        // a kicked punch: its grant is revoked (above) and its XUID's peers go
        for x in &outcome.kicked {
            self.game.kick(*x, ctx.now, out);
        }
        self.game.set_grant_voice_blocks(&outcome.voice_blocks);
        let mut m = Map::new();
        m.insert("admitted".into(), json!(outcome.admitted));
        m.insert("proofGrants".into(), json!(outcome.proof_grants));
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

    /// `world.kick`: every grant of the XUID revoked for 90 s, its FIUP keys zeroed, its peers
    /// dropped in every engine.
    pub fn kick(&mut self, o: Obj, ctx: &mut Ctx, out: &mut Vec<Send>) -> CmdResult {
        let xuid = o.xuid("xuid")?;
        self.gate.lock().revoke(xuid, ctx.now);
        let dropped = self.game.kick(xuid, ctx.now, out);
        let mut m = Map::new();
        m.insert("dropped".into(), json!(dropped));
        Ok(m)
    }

    /// A game-scoped command, through the game module.
    pub fn game_command(&mut self, op: &str, o: Obj, ctx: &mut Ctx) -> CmdResult {
        let mut gctx = GameCtx {
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
        // §12.8b (T-L9): a private official world says so, and names its leader while one leads
        if let Some(v) = game.get("private").filter(|v| v.as_bool() == Some(true)) {
            m.insert("private".into(), v.clone());
            if let Some(l) = game.get("leader").filter(|v| v.is_string()) {
                m.insert("leader".into(), l.clone());
            }
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
            }),
        );
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

    /// The game's events (peer.synced, peer.left, peer.rejected) and the empty/occupied hints,
    /// after anything the world did.
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
        // hints: nobody synced or handshaking (an official world has no host's own game)
        let others = self.game.occupants(0);
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

    /// The `diag` event (at most every 3 s, 2048 per world; `final_one` at close): the FIUP
    /// counters.
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
        let body = obj(json!({
            "worldId": self.id,
            "fiup": {
                "helloGame": c.hello_game, "helloHook": c.hello_hook,
                "responseGame": c.response_game, "responseHook": c.response_hook,
                "challengeGame": c.challenge_game, "challengeHook": c.challenge_hook,
                "ackGame": c.ack_game, "ackHook": c.ack_hook, "rejected": c.rejected,
            },
        }));
        ctx.events.push(events::DIAG, Some(&self.id), body);
    }

    /// A datagram for this world, in the contract's order (§5.2): FIUP, then the game. Every
    /// datagram comes straight from the joiner, and every answer goes straight back.
    pub fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>) {
        if data.len() >= 4 && &data[..4] == b"FIUP" {
            let reply = self.gate.lock().receive_proof(data, from, now);
            if let Some(r) = reply {
                out.push(Send { to: from, data: r });
            }
            return;
        }
        self.game.receive(data, from, now, out);
    }
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
