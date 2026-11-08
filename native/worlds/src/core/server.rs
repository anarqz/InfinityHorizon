// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The server: control sessions and their rules (contract §3.1, §4.2), world commands with their
//! fault boundary (§4.3, §4.6), capacity and the public budget (§8), housekeeping, drain and the
//! process exit decision (§2.1, §2.2).

use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};

use serde_json::{Map, Value, json};

use super::{
    Config, Datagram, Io,
    admission::Shared,
    events::{self, EventQueue},
    fields::{Obj, text_id},
    game::{Admit, OpenInfo, Send},
    keys::KeyRing,
    rng::Rng,
    timers::{Timer, Timers},
    world::{self, Ctx, World},
};
use crate::{
    control::{
        ConnId,
        proto::{self, CmdError, CmdResult},
    },
    log, log_ids,
};

/// The `profile` `hello` answers (contract §3.2): official worlds only (OD-208).
pub const PROFILE: &str = "public";

const HOUSEKEEPING_MS: u64 = 1000;
pub(crate) const MIB: u64 = 1024 * 1024;
/// Public budget: per world `4 MB + seat MB × maxPlayers` against `limit − 20 MB` (§8); the
/// seat share is the game's ([`crate::core::game::Game::seat_reserve_mb`]: 3 MB unless its
/// measured worst case is smaller, FH2 2 MB).
pub(crate) const WORLD_BASE_MB: u64 = 4;
pub(crate) const SEAT_MB: u64 = 3;
pub(crate) const BUDGET_HEADROOM_MB: u64 = 20;
/// Public CPU guard: refuse opens while the rolling 60 s CPU is above 60 % of one core (OD-218,
/// W13: a 1 vCPU host's 100 players are estimated at 20-40 %, so the guard only stops opens near
/// saturation and leaves the rest of the core to the relay, the agent and the OS).
const CPU_GUARD_PERMILLE: u64 = 600;
const CPU_WINDOW_MS: u64 = 60_000;
/// Health: unhealthy when the loop lagged more than 5 s in the last 30 s.
const LAG_UNHEALTHY_MS: u64 = 5000;
const LAG_WINDOW_MS: u64 = 30_000;
const LAG_SAMPLES: usize = 512;
/// A world lapses 45 s after open without context.
const NO_CONTEXT_LAPSE_MS: u64 = super::admission::HOST_LEASE_MS;

#[derive(Debug)]
struct Session {
    /// A request with an `op` was seen (the first one must be `hello`, §2.1).
    seen: bool,
    hello: bool,
    controller: bool,
}

/// Counters for `stats` (contract §2.3).
#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub datagrams_in: u64,
    pub datagrams_out: u64,
    pub datagrams_dropped: u64,
    /// Datagrams the public socket could route to no world (§5.2).
    pub unrouted_dropped: u64,
    pub rate_drops: u64,
    pub send_errors: u64,
    pub refused_opens: u64,
    pub control_queue_high: u64,
    pub world_faults: u64,
}

#[derive(Debug, Clone, Copy)]
struct Drain {
    deadline: u64,
}

pub struct Server {
    cfg: Config,
    keys: KeyRing,
    worlds: BTreeMap<String, World>,
    sessions: BTreeMap<ConnId, Session>,
    controller: Option<ConnId>,
    events: EventQueue,
    timers: Timers,
    rng: Rng,
    start: u64,
    now: u64,
    drain: Option<Drain>,
    exit: Option<i32>,
    closes: Vec<ConnId>,
    out: Vec<Datagram>,
    pub stats: Stats,
    lag_samples: VecDeque<u64>,
    lag_last: u64,
    lag_big_at: Option<u64>,
    cpu_samples: VecDeque<(u64, u64)>,
    memory_high: bool,
    last_stats: u64,
}

fn world_id_of(o: Obj) -> Result<String, CmdError> {
    o.uuid("worldId").map(str::to_string)
}

impl Server {
    pub fn new(cfg: Config, seed: u64, now: u64) -> Server {
        let keys = cfg.keys.clone();
        Server {
            cfg,
            keys,
            worlds: BTreeMap::new(),
            sessions: BTreeMap::new(),
            controller: None,
            events: EventQueue::new(),
            timers: Timers::default(),
            rng: Rng::new(seed),
            start: now,
            now,
            drain: None,
            exit: None,
            closes: Vec::new(),
            out: Vec::new(),
            stats: Stats::default(),
            lag_samples: VecDeque::new(),
            lag_last: 0,
            lag_big_at: None,
            cpu_samples: VecDeque::new(),
            memory_high: false,
            last_stats: now,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn advance(&mut self, now: u64) {
        self.now = self.now.max(now);
    }

    // ----- what the runtime reads back -------------------------------------------------------

    /// The exit code once the process must stop (after flushing replies).
    pub fn exit_code(&self) -> Option<i32> {
        self.exit
    }

    /// Connections the server wants closed.
    pub fn take_closes(&mut self) -> Vec<ConnId> {
        std::mem::take(&mut self.closes)
    }

    pub fn take_datagrams(&mut self) -> Vec<Datagram> {
        std::mem::take(&mut self.out)
    }

    /// The next event line for the controller (with its `seq`), if any.
    pub fn pop_event_line(&mut self) -> Option<String> {
        self.events.pop_line()
    }

    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }

    pub fn events_full(&self) -> bool {
        self.events.is_full()
    }

    pub fn controller(&self) -> Option<ConnId> {
        self.controller
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.timers.next_deadline()
    }

    pub fn world_ids(&self) -> Vec<String> {
        self.worlds.keys().cloned().collect()
    }

    // ----- runtime measurements --------------------------------------------------------------

    /// How late the loop woke for a deadline.
    pub fn note_lag(&mut self, now: u64, lag_ms: u64) {
        self.lag_last = lag_ms;
        if self.lag_samples.len() >= LAG_SAMPLES {
            self.lag_samples.pop_front();
        }
        self.lag_samples.push_back(lag_ms);
        if lag_ms > LAG_UNHEALTHY_MS {
            self.lag_big_at = Some(now);
        }
    }

    pub fn note_sent(&mut self, n: u64) {
        self.stats.datagrams_out += n;
    }

    pub fn note_send_error(&mut self) {
        self.stats.send_errors += 1;
    }

    pub fn note_queue_depth(&mut self, depth: u64) {
        self.stats.control_queue_high = self.stats.control_queue_high.max(depth);
    }

    fn lag_p99(&self) -> u64 {
        if self.lag_samples.is_empty() {
            return 0;
        }
        let mut v: Vec<u64> = self.lag_samples.iter().copied().collect();
        v.sort_unstable();
        v[((v.len() * 99).div_ceil(100))
            .saturating_sub(1)
            .min(v.len() - 1)]
    }

    fn healthy_loop(&self) -> bool {
        self.lag_big_at
            .is_none_or(|t| self.now.saturating_sub(t) >= LAG_WINDOW_MS)
    }

    // ----- sessions ---------------------------------------------------------------------------

    pub fn connect(&mut self, conn: ConnId) {
        self.sessions.insert(
            conn,
            Session {
                seen: false,
                hello: false,
                controller: false,
            },
        );
    }

    /// A connection ended (EOF, error, or closed by us). The worlds stay: a controller that
    /// comes back finds them (§2.2).
    pub fn disconnect(&mut self, now: u64, conn: ConnId) {
        self.advance(now);
        let Some(session) = self.sessions.remove(&conn) else {
            return;
        };
        if !session.controller || self.controller != Some(conn) {
            return;
        }
        self.controller = None;
        self.events.reset(false);
        log::info(log_ids::DS_CONTROL_CLOSED)
            .field("controller", true)
            .emit();
    }

    /// The controller stopped reading for 30 s while its event queue was full (§4.6).
    pub fn controller_stalled(&mut self, now: u64) {
        let Some(conn) = self.controller else { return };
        log::error(log_ids::DS_CONTROL_STALLED).emit();
        self.closes.push(conn);
        self.disconnect(now, conn);
    }

    fn set_exit(&mut self, code: i32) {
        if self.exit.is_none() {
            self.exit = Some(code);
        }
    }

    // ----- requests ---------------------------------------------------------------------------

    /// One request line (a JSON object) from `conn`; returns its reply.
    pub fn request(&mut self, io: &mut dyn Io, now: u64, conn: ConnId, request: &Value) -> Value {
        self.advance(now);
        let id = match proto::request_id(request) {
            Some(n) => json!(n),
            None => return proto::reply_err(&Value::Null, &CmdError::bad("id")),
        };
        let Some(map) = request.as_object() else {
            return proto::reply_err(&id, &CmdError::bad("json"));
        };
        let o = Obj::new(map, "");
        let Ok(op) = o.str("op") else {
            return proto::reply_err(&id, &CmdError::bad("op"));
        };
        let Some(session) = self.sessions.get_mut(&conn) else {
            return proto::reply_err(&id, &CmdError::code(proto::NOT_READY));
        };
        let first = !std::mem::replace(&mut session.seen, true);
        if !session.hello {
            if op != "hello" && first {
                // §2.1: a first message that is not `hello` fails authentication; the
                // connection closes (never the engine)
                log::error(log_ids::DS_CONTROL_UNAUTHORIZED).emit();
                self.closes.push(conn);
                return proto::reply_err(&id, &CmdError::code(proto::UNAUTHORIZED));
            }
            if op != "hello" {
                return proto::reply_err(&id, &CmdError::code(proto::NOT_READY));
            }
            let r = self.hello(conn, o);
            return proto::reply(&id, r);
        }
        if op == "hello" {
            return proto::reply_err(&id, &CmdError::bad("op"));
        }
        if !session.controller && !matches!(op, "health" | "capacity" | "stats") {
            return proto::reply_err(&id, &CmdError::bad("op").reason("controller_only"));
        }
        let r = match op {
            "keys.set" => self.keys_set(o),
            "capacity" => Ok(self.capacity_value(io)),
            "health" => Ok(self.health_value(io)),
            "stats" => Ok(self.stats_value(io)),
            "drain" => self.drain_cmd(io, o),
            "shutdown" => self.shutdown_cmd(io),
            "catalog.set" | "catalog.parts" => self.game_cmd(op, o),
            "world.open" => self.world_open(io, o),
            "world.close" => self.world_close(io, o),
            "world.context"
            | "world.admission"
            | "world.admission.end"
            | "world.voice"
            | "world.voiceBlocks"
            | "world.watch"
            | "world.kick"
            | "world.status"
            | "world.gameSession.answer" => self.world_cmd(io, op, o),
            #[cfg(feature = "test-hooks")]
            "debug.fault" => self.world_cmd(io, op, o),
            _ => Err(CmdError::code(proto::UNKNOWN_OP)),
        };
        proto::reply(&id, r)
    }

    /// A game's process-level command (`catalog.set`, `catalog.parts`; contract §9).
    fn game_cmd(&mut self, op: &str, o: Obj) -> CmdResult {
        let game = crate::core::world::game_of(o)?;
        let Some(g) = self.cfg.games.iter().find(|g| g.id() == game).cloned() else {
            return Err(o.bad("game"));
        };
        g.command(op, o)
            .unwrap_or_else(|| Err(CmdError::code(proto::UNKNOWN_OP)))
    }

    fn hello(&mut self, conn: ConnId, o: Obj) -> CmdResult {
        // the contract's integer (`proto: 1`); the step-1 range form `[lo, hi]` is still read
        let range = match o.get("proto") {
            Some(Value::Number(n)) => n
                .as_u64()
                .filter(|v| *v <= u64::from(u32::MAX))
                .map(|v| (v, v)),
            Some(Value::Array(a)) if a.len() == 2 => {
                a[0].as_u64().zip(a[1].as_u64()).filter(|(lo, hi)| lo <= hi)
            }
            _ => None,
        }
        .ok_or_else(|| CmdError::bad("proto"))?;
        let controller = if o.has("controller") {
            let c = Obj::new(o.obj("controller")?, "controller");
            // the Worlds host's agent; the launcher controls no world any more (OD-208)
            if c.str("kind")? != "vm" {
                return Err(c.bad("kind"));
            }
            let v = c.str("version")?;
            if v.is_empty() || v.len() > 64 {
                return Err(c.bad("version"));
            }
            true
        } else {
            false
        };
        // a second controller while one is attached is only ever `busy`: nothing it sends (a bad
        // proto included) may end the process the attached controller is using
        if controller && self.controller.is_some_and(|c| c != conn) {
            return Err(CmdError::busy(proto::BUSY_RETRY_MS));
        }
        let lo = range.0.max(crate::PROTO_MIN as u64);
        let hi = range.1.min(crate::PROTO_MAX as u64);
        if lo > hi {
            log::error(log_ids::DS_CONTROL_PROTO_UNSUPPORTED).emit();
            if controller {
                self.closes.push(conn);
                self.set_exit(5);
            }
            return Err(CmdError::code(proto::PROTO_UNSUPPORTED));
        }
        if let Some(s) = self.sessions.get_mut(&conn) {
            s.hello = true;
            s.controller = controller;
        }
        if controller {
            self.controller = Some(conn);
            self.events.reset(true);
            log::info(log_ids::DS_CONTROL_CONNECTED)
                .field("proto", hi)
                .emit();
        }
        let mut m = Map::new();
        m.insert("proto".into(), json!(hi));
        m.insert(
            "build".into(),
            json!({"version": crate::VERSION, "commit": crate::commit()}),
        );
        m.insert("profile".into(), json!(PROFILE));
        m.insert("capacity".into(), Value::Object(self.capacity_map(None)));
        m.insert("draining".into(), Value::Bool(self.drain.is_some()));
        m.insert("worlds".into(), json!(self.world_ids()));
        m.insert("keys".into(), Value::Bool(!self.keys.is_empty()));
        Ok(m)
    }

    fn keys_set(&mut self, o: Obj) -> CmdResult {
        let ring = KeyRing::parse(o.str("ring")?).map_err(|_| CmdError::bad("ring"))?;
        self.keys = ring;
        let mut m = Map::new();
        m.insert("count".into(), json!(self.keys.len()));
        Ok(m)
    }

    // ----- capacity, health, stats ------------------------------------------------------------

    fn seats(&self) -> usize {
        self.worlds
            .values()
            .map(|w| w.spec.max_players as usize)
            .sum()
    }

    fn reserved_mb(&self) -> u64 {
        self.worlds.values().map(|w| w.reserved_mb).sum()
    }

    fn budgeted(&self) -> bool {
        self.cfg.memory_limit_mb > 0
    }

    fn used_mb(&self, io: Option<&dyn Io>) -> u64 {
        io.map_or(0, |io| io.memory_used_bytes().div_ceil(MIB))
    }

    fn cpu_guard(&self) -> bool {
        let (Some(first), Some(last)) = (self.cpu_samples.front(), self.cpu_samples.back()) else {
            return false;
        };
        let wall = last.0.saturating_sub(first.0);
        wall >= 1000 && last.1.saturating_sub(first.1) * 1000 > wall * CPU_GUARD_PERMILLE
    }

    fn sample_cpu(&mut self, io: &dyn Io) {
        let Some(cpu) = io.cpu_ms() else { return };
        self.cpu_samples.push_back((self.now, cpu));
        while self
            .cpu_samples
            .front()
            .is_some_and(|(t, _)| self.now.saturating_sub(*t) > CPU_WINDOW_MS)
        {
            self.cpu_samples.pop_front();
        }
    }

    fn capacity_map(&self, io: Option<&dyn Io>) -> Map<String, Value> {
        let seats = self.seats();
        let mut m = Map::new();
        m.insert("maxWorlds".into(), json!(self.cfg.max_worlds));
        m.insert("maxSeats".into(), json!(self.cfg.max_seats));
        m.insert("openWorlds".into(), json!(self.worlds.len()));
        m.insert("seats".into(), json!(seats));
        m.insert(
            "freeSeats".into(),
            json!(self.cfg.max_seats.saturating_sub(seats)),
        );
        if self.budgeted() {
            m.insert(
                "memory".into(),
                json!({
                    "usedMb": self.used_mb(io),
                    "limitMb": self.cfg.memory_limit_mb,
                    "reservedMb": self.reserved_mb(),
                }),
            );
        }
        m.insert("cpuGuard".into(), Value::Bool(self.cpu_guard()));
        m
    }

    fn capacity_value(&self, io: &dyn Io) -> Map<String, Value> {
        self.capacity_map(Some(io))
    }

    fn ready(&self) -> bool {
        self.cfg.public_socket && self.budgeted() && !self.keys.is_empty() && self.healthy_loop()
    }

    fn health_value(&self, io: &dyn Io) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("ready".into(), Value::Bool(self.ready()));
        m.insert(
            "loopLagMs".into(),
            json!({"last": self.lag_last, "p99": self.lag_p99()}),
        );
        m.insert(
            "uptimeMs".into(),
            json!(self.now.saturating_sub(self.start)),
        );
        m.insert("worlds".into(), json!(self.worlds.len()));
        m.insert("seats".into(), json!(self.seats()));
        if self.budgeted() {
            m.insert(
                "memory".into(),
                json!({"usedMb": self.used_mb(Some(io)), "limitMb": self.cfg.memory_limit_mb}),
            );
        }
        m
    }

    fn stats_value(&self, io: &dyn Io) -> Map<String, Value> {
        let synced: usize = self.worlds.values().map(World::synced).sum();
        let mut m = Map::new();
        m.insert("worlds".into(), json!(self.worlds.len()));
        m.insert("seats".into(), json!(self.seats()));
        m.insert("synced".into(), json!(synced));
        m.insert(
            "caps".into(),
            json!({"maxWorlds": self.cfg.max_worlds, "maxSeats": self.cfg.max_seats}),
        );
        m.insert(
            "memory".into(),
            json!({"usedMb": self.used_mb(Some(io)), "limitMb": self.cfg.memory_limit_mb}),
        );
        m.insert(
            "rssMb".into(),
            json!(io.rss_bytes().map(|b| b.div_ceil(MIB))),
        );
        m.insert("cpuMs".into(), json!(io.cpu_ms()));
        m.insert("loopLagP99Ms".into(), json!(self.lag_p99()));
        m.insert("datagramsIn".into(), json!(self.stats.datagrams_in));
        m.insert("datagramsOut".into(), json!(self.stats.datagrams_out));
        m.insert(
            "datagramsDropped".into(),
            json!(self.stats.datagrams_dropped),
        );
        m.insert("rateDrops".into(), json!(self.stats.rate_drops));
        m.insert("sendErrors".into(), json!(self.stats.send_errors));
        m.insert("refusedOpens".into(), json!(self.stats.refused_opens));
        m.insert(
            "controlQueueHigh".into(),
            json!(self.stats.control_queue_high),
        );
        m.insert("eventsDropped".into(), json!(self.events.dropped_total));
        m.insert("logDropped".into(), json!(log::dropped()));
        m.insert("worldFaults".into(), json!(self.stats.world_faults));
        m
    }

    fn log_stats(&mut self, io: &dyn Io) {
        self.last_stats = self.now;
        let mut line = log::info(log_ids::DS_STATS);
        for (k, v) in self.stats_value(io) {
            line = line.field(&k, v);
        }
        line.emit();
    }

    // ----- process commands -------------------------------------------------------------------

    fn drain_cmd(&mut self, io: &mut dyn Io, o: Obj) -> CmdResult {
        let secs = o.opt_int("deadlineSec", 0, 300, self.cfg.drain_seconds)?;
        self.start_drain(io, secs);
        Ok(Map::new())
    }

    /// Starts a drain (the `drain` command or SIGTERM); a second call ends it at once.
    pub fn start_drain(&mut self, io: &mut dyn Io, secs: u64) {
        if self.drain.is_some() {
            self.finish_drain(io);
            return;
        }
        let deadline = self.now + secs * 1000;
        self.drain = Some(Drain { deadline });
        self.events.push(
            events::DS_DRAINING,
            None,
            obj(json!({"deadlineMs": secs * 1000})),
        );
        log::info(log_ids::DS_DRAINING)
            .field("deadlineMs", secs * 1000)
            .emit();
        self.timers.reschedule(Timer::Drain, deadline);
        self.check_drain(io);
    }

    /// Ends at once (SIGINT, a second SIGTERM): as `shutdown`.
    pub fn stop_now(&mut self, io: &mut dyn Io) {
        for id in self.world_ids() {
            self.close_world(io, &id, "shutdown", None, true);
        }
        self.set_exit(0);
    }

    fn check_drain(&mut self, io: &mut dyn Io) {
        let Some(d) = self.drain else { return };
        let all_empty = self
            .worlds
            .values()
            .all(|w| w.synced() == 0 && w.handshaking() == 0);
        if all_empty || self.now >= d.deadline {
            self.finish_drain(io);
        }
    }

    fn finish_drain(&mut self, io: &mut dyn Io) {
        for id in self.world_ids() {
            self.close_world(io, &id, "drained", None, true);
        }
        self.timers.cancel(&Timer::Drain);
        self.set_exit(0);
    }

    fn shutdown_cmd(&mut self, io: &mut dyn Io) -> CmdResult {
        self.stop_now(io);
        Ok(Map::new())
    }

    // ----- worlds -------------------------------------------------------------------------------

    fn refuse(&mut self, reason: &str) -> CmdError {
        self.stats.refused_opens += 1;
        self.events.push(
            events::NOTICE,
            None,
            obj(json!({"id": events::N_CAPACITY_REFUSED, "level": "warn", "args": {"reason": reason}})),
        );
        log::warn(log_ids::DS_CAPACITY_REFUSED)
            .field("reason", reason)
            .emit();
        CmdError::code(proto::CAPACITY).reason(reason)
    }

    fn world_open(&mut self, io: &mut dyn Io, o: Obj) -> CmdResult {
        let id = world_id_of(o)?;
        // the game's own seat ceiling (FH2 12, FM5 16); an unknown game is refused below
        let cap = world::game_of(o)
            .ok()
            .and_then(|g| self.cfg.games.iter().find(|x| x.id() == g))
            .map_or(world::MAX_PLAYERS_DEFAULT, |g| g.max_players());
        let spec = world::parse_open(o, cap)?;
        let Some(game) = self.cfg.games.iter().find(|g| g.id() == spec.game).cloned() else {
            return Err(CmdError::bad("game"));
        };
        let gate = Shared::new(World::rules(&id, &spec));
        let admit: Arc<dyn Admit> = Arc::new(gate.clone());
        let info = |rng| OpenInfo {
            request: o,
            max_players: spec.max_players,
            rng,
            admit: admit.clone(),
        };
        if let Some(w) = self.worlds.get(&id) {
            // the game side normalizes its own fields; a scratch RNG keeps the server's untouched
            let mut scratch = self.rng.clone();
            let (candidate, _) = game.open(info(&mut scratch))?;
            return if w.spec == spec && w.game.spec() == candidate.spec() {
                Ok(w.open_reply.clone())
            } else {
                Err(CmdError::code(proto::EXISTS))
            };
        }
        if self.drain.is_some() {
            return Err(CmdError::code(proto::DRAINING));
        }
        if self.keys.is_empty() {
            return Err(CmdError::code(proto::NO_KEYS));
        }
        if self.worlds.len() >= self.cfg.max_worlds {
            return Err(self.refuse("worlds"));
        }
        if self.seats() + spec.max_players as usize > self.cfg.max_seats {
            return Err(self.refuse("seats"));
        }
        let reserve = if self.budgeted() {
            WORLD_BASE_MB + game.seat_reserve_mb() * spec.max_players as u64
        } else {
            0
        };
        if self.budgeted() {
            self.check_memory(io);
            let budget = self.cfg.memory_limit_mb.saturating_sub(BUDGET_HEADROOM_MB);
            if self.memory_high || self.reserved_mb() + reserve > budget {
                return Err(self.refuse("memory"));
            }
        }
        self.sample_cpu(io);
        if self.cpu_guard() {
            self.events.push(
                events::NOTICE,
                None,
                obj(json!({"id": events::N_CPU_GUARD, "level": "warn"})),
            );
            return Err(self.refuse("cpu"));
        }
        let mut rng = self.rng.clone();
        let (game_world, game_notices) = game.open(info(&mut rng))?;
        self.rng = rng;
        let mut w = World::new(&id, spec, game_world, gate, self.now);
        w.reserved_mb = reserve;
        let mut reply = Map::new();
        reply.insert("worldId".into(), json!(id));
        reply.insert("lobbies".into(), json!(w.game.lobbies()));
        w.open_reply = reply.clone();
        for n in game_notices {
            let mut body = obj(json!({"id": n.id, "level": n.level, "worldId": id}));
            if let Some(a) = n.args {
                body.insert("args".into(), a);
            }
            self.events.push(events::NOTICE, Some(&id), body);
        }
        log::info(log_ids::WORLD_OPENED)
            .world(&id)
            .field("game", w.spec.game.as_str())
            .field("maxPlayers", w.spec.max_players)
            .emit();
        self.worlds.insert(id.clone(), w);
        self.timers
            .schedule(Timer::Housekeeping, self.now + HOUSEKEEPING_MS);
        if !self.after_world_guarded(io, &id) {
            return Err(CmdError::code(proto::INTERNAL));
        }
        Ok(reply)
    }

    fn world_close(&mut self, io: &mut dyn Io, o: Obj) -> CmdResult {
        let id = world_id_of(o)?;
        let reason = o.str("reason")?;
        if !text_id(reason) {
            return Err(o.bad("reason"));
        }
        if self.worlds.contains_key(&id) {
            let (closed_reason, send_close) = match reason {
                "shutdown" => ("shutdown", true),
                "drained" => ("drained", true),
                _ => ("controller", false),
            };
            self.close_world(io, &id, closed_reason, None, send_close);
        }
        Ok(Map::new())
    }

    /// The world commands that run inside a world's fault boundary.
    fn world_cmd(&mut self, io: &mut dyn Io, op: &str, o: Obj) -> CmdResult {
        let id = world_id_of(o)?;
        let Server {
            worlds,
            events,
            timers,
            keys,
            now,
            ..
        } = self;
        let Some(w) = worlds.get_mut(&id) else {
            return Err(CmdError::code(proto::NOT_FOUND));
        };
        let mut ctx = Ctx {
            keys_empty: keys.is_empty(),
            keys,
            events,
            timers,
            now: *now,
        };
        let mut sends = Vec::new();
        let result = catch_unwind(AssertUnwindSafe(|| -> CmdResult {
            match op {
                "world.context" => w.context(o, &mut ctx, &mut sends),
                "world.admission" => w.admission(o, &mut ctx, &mut sends),
                "world.admission.end" => w.admission_end(o, &mut ctx),
                "world.kick" => w.kick(o, &mut ctx, &mut sends),
                "world.status" => match w.state(ctx.now) {
                    Value::Object(m) => Ok(m),
                    _ => Ok(Map::new()),
                },
                #[cfg(feature = "test-hooks")]
                "debug.fault" => {
                    // `where: "after"`: the world's follow-up (events, state) panics instead
                    if o.get("where").and_then(Value::as_str) == Some("after") {
                        w.fault_after = true;
                        Ok(Map::new())
                    } else {
                        panic!("debug.fault")
                    }
                }
                _ => w.game_command(op, o, &mut ctx),
            }
        }));
        match result {
            Ok(r) => {
                push_sends(&mut self.out, sends);
                if !self.after_world_guarded(io, &id) {
                    return Err(CmdError::code(proto::INTERNAL));
                }
                r
            }
            Err(_) => {
                self.fault(io, &id, "world.panic.command");
                Err(CmdError::code(proto::INTERNAL))
            }
        }
    }

    /// `after_world` inside the world's fault boundary: a panic there faults only that world.
    /// False when it did.
    fn after_world_guarded(&mut self, io: &mut dyn Io, id: &str) -> bool {
        if catch_unwind(AssertUnwindSafe(|| self.after_world(id))).is_ok() {
            return true;
        }
        self.fault(io, id, "world.panic.after");
        false
    }

    /// After anything changed a world: its state event and its engine tick.
    fn after_world(&mut self, id: &str) {
        let Server {
            worlds,
            events,
            timers,
            keys,
            now,
            ..
        } = self;
        let Some(w) = worlds.get_mut(id) else { return };
        let mut ctx = Ctx {
            keys_empty: keys.is_empty(),
            keys,
            events,
            timers,
            now: *now,
        };
        w.emit_events(&mut ctx);
        w.emit_state(&mut ctx, false);
        match w.game.next_tick(*now) {
            Some(at) => ctx.timers.schedule(Timer::EngineTick(id.to_string()), at),
            None => ctx.timers.cancel(&Timer::EngineTick(id.to_string())),
        }
    }

    /// A world's fault: close only that world, with a reason (§4.6).
    fn fault(&mut self, io: &mut dyn Io, id: &str, fault: &str) {
        self.stats.world_faults += 1;
        log::error(log_ids::WORLD_FAULT)
            .world(id)
            .field("fault", fault)
            .emit();
        self.close_world(io, id, "fault", Some(fault), false);
    }

    fn close_world(
        &mut self,
        io: &mut dyn Io,
        id: &str,
        reason: &str,
        fault: Option<&str>,
        send_close: bool,
    ) {
        let Some(mut w) = self.worlds.remove(id) else {
            return;
        };
        if send_close {
            let mut sends = Vec::new();
            let closing = catch_unwind(AssertUnwindSafe(|| {
                w.game.close_all(&mut sends);
            }));
            if closing.is_ok() {
                push_sends(&mut self.out, sends);
            }
        }
        {
            let mut ctx = Ctx {
                keys_empty: self.keys.is_empty(),
                keys: &self.keys,
                events: &mut self.events,
                timers: &mut self.timers,
                now: self.now,
            };
            // the last diag runs in the fault boundary too: a panic there loses only that line
            let _ = catch_unwind(AssertUnwindSafe(|| w.diag(&mut ctx, true)));
            w.gate.lock().clear();
        }
        self.timers.cancel(&Timer::StateFlush(id.to_string()));
        self.timers.cancel(&Timer::EngineTick(id.to_string()));
        let mut body = obj(json!({"worldId": id, "reason": reason}));
        if let Some(f) = fault {
            body.insert("fault".into(), json!(f));
        }
        self.events.push(events::WORLD_CLOSED, Some(id), body);
        let mut line = log::info(log_ids::WORLD_CLOSED)
            .world(id)
            .field("reason", reason);
        if let Some(f) = fault {
            line = line.field("fault", f);
        }
        line.emit();
        if self.worlds.is_empty() {
            self.timers.cancel(&Timer::Housekeeping);
            if self.cfg.stats_seconds > 0 {
                self.log_stats(io);
            }
        }
    }

    // ----- datagrams and timers ---------------------------------------------------------------

    /// A datagram received on the public socket from `from`: one socket for every world,
    /// joiners straight to it (§5.2, OD-189).
    pub fn datagram(&mut self, io: &mut dyn Io, now: u64, from: SocketAddr, data: &[u8]) {
        self.advance(now);
        self.stats.datagrams_in += 1;
        let from = crate::stun::plain_endpoint(from);
        let Some(id) = self.public_owner(data, from) else {
            self.stats.unrouted_dropped += 1;
            self.stats.datagrams_dropped += 1;
            return;
        };
        self.world_datagram(io, &id, from, data);
    }

    /// The public socket's routing (§5.2): a FIUP frame goes to the world holding its grant id
    /// (bytes 8..24 of the header), any other datagram to the world where its source endpoint
    /// holds a fresh FIUP binding. Anything else (an unproven source, a FIUP frame for no
    /// world's grant) has no world and is dropped.
    fn public_owner(&self, data: &[u8], from: SocketAddr) -> Option<String> {
        let now = self.now;
        if data.len() >= 24 && &data[..4] == b"FIUP" {
            let mut grant = [0u8; 16];
            grant.copy_from_slice(&data[8..24]);
            return self
                .worlds
                .iter()
                .find(|(_, w)| w.gate.lock().holds_grant(&grant))
                .map(|(k, _)| k.clone());
        }
        self.worlds
            .iter()
            .find(|(_, w)| w.gate.lock().binds(from, now))
            .map(|(k, _)| k.clone())
    }

    fn world_datagram(&mut self, io: &mut dyn Io, id: &str, from: SocketAddr, data: &[u8]) {
        let id = id.to_string();
        let Some(w) = self.worlds.get_mut(&id) else {
            return;
        };
        let mut sends = Vec::new();
        let before = w.game.dropped();
        let r = catch_unwind(AssertUnwindSafe(|| {
            w.receive(data, from, self.now, &mut sends)
        }));
        match r {
            Ok(()) => {
                let after = w.game.dropped();
                self.stats.datagrams_dropped += after - before;
                push_sends(&mut self.out, sends);
                self.after_world_guarded(io, &id);
            }
            Err(_) => self.fault(io, &id, "world.panic.datagram"),
        }
    }

    /// Runs every timer due at `now`.
    pub fn tick(&mut self, io: &mut dyn Io, now: u64) {
        self.advance(now);
        for t in self.timers.pop_due(self.now) {
            match t {
                Timer::Housekeeping => self.housekeeping(io),
                Timer::StateFlush(id) => {
                    self.after_world_guarded(io, &id);
                }
                Timer::EngineTick(id) => self.engine_tick(io, &id),
                Timer::Drain => self.check_drain(io),
            }
        }
    }

    fn engine_tick(&mut self, io: &mut dyn Io, id: &str) {
        let Some(w) = self.worlds.get_mut(id) else {
            return;
        };
        let mut sends = Vec::new();
        let now = self.now;
        match catch_unwind(AssertUnwindSafe(|| w.game.tick(now, &mut sends))) {
            Ok(()) => {
                push_sends(&mut self.out, sends);
                self.after_world_guarded(io, id);
            }
            Err(_) => self.fault(io, id, "world.panic.tick"),
        }
    }

    fn check_memory(&mut self, io: &dyn Io) {
        if !self.budgeted() {
            return;
        }
        let used = self.used_mb(Some(io));
        let limit = self.cfg.memory_limit_mb;
        if !self.memory_high && used * 10 > limit * 9 {
            self.memory_high = true;
            self.events.push(
                events::NOTICE,
                None,
                obj(json!({"id": events::N_MEMORY_HIGH, "level": "warn",
                           "args": {"usedMb": used, "limitMb": limit}})),
            );
            log::warn(log_ids::DS_MEMORY_HIGH)
                .field("usedMb", used)
                .field("limitMb", limit)
                .emit();
        } else if self.memory_high && used * 10 < limit * 8 {
            self.memory_high = false;
        }
    }

    fn housekeeping(&mut self, io: &mut dyn Io) {
        self.sample_cpu(io);
        self.check_memory(io);
        let now = self.now;
        let controller = self.controller.is_some();
        let mut lapsed_close = Vec::new();
        for id in self.world_ids() {
            let Server {
                worlds,
                events,
                timers,
                keys,
                ..
            } = self;
            let Some(w) = worlds.get_mut(&id) else {
                continue;
            };
            let mut ctx = Ctx {
                keys_empty: keys.is_empty(),
                keys,
                events,
                timers,
                now,
            };
            w.gate.lock().housekeep(now);
            let until = w.lease_until();
            let lapsed = match until {
                Some(_) => !w.lease_live(now),
                None => now.saturating_sub(w.opened_at) >= NO_CONTEXT_LAPSE_MS,
            };
            if lapsed && until.is_some() && !w.admission.lapse_noticed {
                w.admission.lapse_noticed = true;
                ctx.notice(events::N_ADMISSION_LAPSED, "warn", Some(&id), None);
                log::warn(log_ids::WORLD_ADMISSION)
                    .world(&id)
                    .field("lapsed", true)
                    .emit();
            }
            // with no controller, a lapsed world with nobody synced closes itself (§2.2)
            if lapsed && !controller && w.synced() == 0 {
                lapsed_close.push(id.clone());
                continue;
            }
            w.emit_state(&mut ctx, true);
        }
        for id in lapsed_close {
            self.close_world(io, &id, "admission_lapsed", None, false);
        }
        if self.cfg.stats_seconds > 0
            && now.saturating_sub(self.last_stats) >= self.cfg.stats_seconds * 1000
        {
            self.log_stats(io);
        }
        self.check_drain(io);
        if !self.worlds.is_empty() {
            self.timers
                .schedule(Timer::Housekeeping, now + HOUSEKEEPING_MS);
        }
    }
}

fn push_sends(out: &mut Vec<Datagram>, sends: Vec<Send>) {
    out.extend(sends.into_iter().map(|s| Datagram {
        to: s.to,
        data: s.data,
    }));
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}
