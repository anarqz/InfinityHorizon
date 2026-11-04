// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The agent's supervisor and placement loop (DEDICATED_SERVER.md §16; WORLDS_OFFICIAL.md §4, §5;
//! MATCHMAKING_API §2.9):
//!
//! 1. `GET /mm/v1/official/hello` (retried with backoff): the server's clock and grant keys;
//! 2. the engine child, its controller `hello`, `keys.set`, `catalog.set`; restarted with backoff
//!    when it ends, its worlds `DELETE`d on the server;
//! 3. one spare open world per (mode, activity) for `freeroam` and `roadtrip` and one `carmeet`
//!    spare for both modes (WORLDS_OFFICIAL.md §11.1): `world.open` + `PUT` with a client-minted
//!    id kept for its retries; an announce per world every 15 s with `world.state`'s peers, each
//!    answer feeding `world.context` then `world.admission`; an idle extra world closed (`DELETE`);
//! 4. `catalog.need` answered with `catalog.parts`;
//! 5. a car meet's `world.gameSession.request` answered from `POST …/game-sessions` (a call off
//!    the loop, at most 14 s; DEDICATED_SERVER.md §4.4), `session: null` when refused or late;
//! 6. SIGTERM/SIGINT: `DELETE` every world, `shutdown` the engine, exit 0.

use std::{
    collections::HashMap,
    process::Child,
    sync::{
        Arc,
        mpsc::{Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};

use super::{
    catalog::{self, Catalog},
    config::{self, AgentConfig, GAME},
    engine::{self, Control, Input, MadeSession, SessionAnswer},
    exit,
    http::{Client, Reply},
    sign, uuid_v4,
};
use crate::{core::keys::KeyRing, log, log_ids};

const HELLO: &str = "/mm/v1/official/hello";
/// How often the hello is read again (clock offset, key rotation, activities).
const HELLO_REFRESH: Duration = Duration::from_secs(300);
/// An extra world (beyond its group's one spare) empty this long is closed.
const IDLE_CLOSE: Duration = Duration::from_secs(60);
/// A failed announce is retried this soon (the server's TTL is 45 s).
const ANNOUNCE_RETRY: Duration = Duration::from_secs(5);
/// The most attempts of one `DELETE` (the server's TTL closes the world anyway).
const DELETE_TRIES: u32 = 10;
/// The longest backoff of a hello, an open or an engine restart.
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// An engine that ran this long restarts after the shortest delay again.
const STABLE_RUN: Duration = Duration::from_secs(120);
/// The most punches in one `world.admission` (§4.3).
const PUNCHES_MAX: usize = 256;
/// A car meet session's budget: `CarMeetCreateMs` 15 s − 1 s (DEDICATED_SERVER.md §4.4).
const GAME_SESSION_BUDGET: Duration = Duration::from_secs(14);
/// The most game-session calls in flight (an engine holds at most 16 requests per world).
const GAME_SESSIONS_MAX: usize = 32;
/// A group world nobody reached is closed after the seats' longest hold (`FI_MM_SEAT_HOLD_SECS`
/// default 600, MATCHMAKING_API §3.2) and a minute empty; the server ends it then anyway.
const GROUP_HOLD: Duration = Duration::from_secs(600);
/// `GameModeSessionRequest.xuids`: 1-12 (MATCHMAKING_API §3.8).
const GAME_SESSION_XUIDS_MAX: usize = 12;

struct World {
    id: String,
    mode: &'static str,
    activity: &'static str,
    peers: Vec<String>,
    /// Admission holds grants or a handshaking peer: someone is on the way in.
    busy: bool,
    idle_since: Option<Instant>,
    next_announce: Instant,
    /// One key per logical announce, kept for its retries.
    announce_key: String,
    last_admission: Option<(usize, u64)>,
    /// A car meet session's group world (WORLDS_OFFICIAL.md §11.4): never a spare.
    group: bool,
    /// Somebody was in it once (a group world closes when it empties again).
    seen_peers: bool,
    /// OD-194: its activity is past the point newcomers may join (a Road Trip past its vote):
    /// announced, so matchmaking places nobody else in it, and not counted as the spare.
    in_progress: bool,
    opened: Instant,
}

/// A `world.gameSession.request` waiting for its call.
struct PendingSession {
    world: String,
    rid: u64,
    deadline: Instant,
}

struct Deletion {
    id: String,
    next: Instant,
    tries: u32,
}

enum Outcome {
    /// The engine ended (exit, EOF, a refused setup).
    Ended,
    /// A signal: stop everything.
    Stop,
    /// Another controller holds the engine.
    Busy,
}

struct Agent {
    cfg: AgentConfig,
    client: Client,
    /// The game-session calls' client (off the loop; 15 s timeout, past the 14 s budget).
    session_client: Arc<Client>,
    /// The loop's input, for the game-session calls' answers.
    inputs: Sender<Input>,
    /// The engine session the loop drives now (answers of an ended one are dropped).
    generation: u64,
    sessions: Vec<PendingSession>,
    offset_ms: i64,
    ring: String,
    hello_activities: Vec<String>,
    activities: Vec<&'static str>,
    catalog: Option<Catalog>,
    road_trip: Option<Map<String, Value>>,
    worlds: Vec<World>,
    deletions: Vec<Deletion>,
    /// The minted id of the next world of a (mode, activity), reused by its retries.
    minted: HashMap<(&'static str, &'static str), String>,
    open_after: Instant,
    open_failures: u32,
    server_full_until: Option<Instant>,
    engine_full: bool,
    engine_cap: usize,
    next_hello: Instant,
    capacity_logged: bool,
}

pub fn run(cfg: AgentConfig) -> i32 {
    let (key, created) = match config::load_or_create_key(&cfg.key_file) {
        Ok(k) => k,
        Err(_) => {
            log::error(log_ids::AGENT_CONFIG_INVALID)
                .field("field", "FI_WORLDS_KEY_FILE")
                .emit();
            return exit::CONFIG;
        }
    };
    if created {
        log::info(log_ids::AGENT_KEY_CREATED).emit();
    }
    // the public half goes into the server's FI_WORLDS; the seed is never logged
    log::info(log_ids::AGENT_STARTED)
        .field("vmId", cfg.vm_id.clone())
        .field("publicKey", sign::public_key(&key))
        .field("version", crate::VERSION)
        .emit();
    let mut catalog = None;
    let mut road_trip = None;
    if let Some(path) = &cfg.catalog {
        let Ok(c) = Catalog::open(path) else {
            log::error(log_ids::AGENT_CONFIG_INVALID)
                .field("field", "FI_WORLDS_CATALOG")
                .emit();
            return exit::CONFIG;
        };
        let events = c.road_trip().unwrap_or_default();
        log::info(log_ids::AGENT_CATALOG)
            .field("events", events.len())
            .field("parts", c.has_parts())
            .emit();
        if !events.is_empty() {
            road_trip = Some(catalog::catalog_set(&events));
        }
        catalog = Some(c);
    } else {
        log::info(log_ids::AGENT_CATALOG)
            .field("events", 0)
            .field("parts", false)
            .emit();
    }
    let (tx, rx) = engine::channel();
    signals::install(tx.clone());
    let now = Instant::now();
    let mut agent = Agent {
        client: Client::new(&cfg.server.base, &cfg.vm_id, key.clone()),
        session_client: Arc::new(Client::with_timeout(
            &cfg.server.base,
            &cfg.vm_id,
            key,
            GAME_SESSION_BUDGET + Duration::from_secs(1),
        )),
        inputs: tx.clone(),
        generation: 0,
        sessions: Vec::new(),
        cfg,
        offset_ms: 0,
        ring: String::new(),
        hello_activities: Vec::new(),
        activities: Vec::new(),
        catalog,
        road_trip,
        worlds: Vec::new(),
        deletions: Vec::new(),
        minted: HashMap::new(),
        open_after: now,
        open_failures: 0,
        server_full_until: None,
        engine_full: false,
        engine_cap: 0,
        next_hello: now + HELLO_REFRESH,
        capacity_logged: false,
    };
    let mut delay = Duration::from_secs(1);
    while !agent.hello() {
        if wait_signal(&rx, delay) {
            return agent.stopped();
        }
        delay = (delay * 2).min(BACKOFF_MAX);
    }
    agent.supervise(&tx, &rx)
}

fn local_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Waits `d`, true when a signal came meanwhile (other inputs are stale and dropped).
fn wait_signal(rx: &Receiver<Input>, d: Duration) -> bool {
    let until = Instant::now() + d;
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match rx.recv_timeout(left) {
            Ok(Input::Signal) | Err(RecvTimeoutError::Disconnected) => return true,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => return false,
        }
    }
}

/// `details.reason` of a platform error body (`{code, message, details}`).
fn error_reason(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("details")?
        .get("reason")?
        .as_str()
        .map(|s| s.chars().take(32).collect())
}

fn backoff(failures: u32) -> Duration {
    Duration::from_secs(1u64 << failures.min(6)).min(BACKOFF_MAX)
}

impl Agent {
    fn server_ms(&self) -> i64 {
        local_ms() + self.offset_ms
    }

    /// The header's time: the server's clock as the hello measured it.
    fn seconds(&self) -> u64 {
        (self.server_ms().max(0) / 1000) as u64
    }

    fn world_target(id: &str) -> String {
        format!("/mm/v1/official/worlds/{id}")
    }

    /// Logs a refused call by route and status (never its body or URL).
    fn refused(&mut self, route: &str, reply: &Reply) {
        match reply.status {
            401 => {
                log::warn(log_ids::AGENT_UNAUTHORIZED)
                    .field("route", route)
                    .emit();
            }
            429 => {
                log::warn(log_ids::AGENT_THROTTLED)
                    .field("route", route)
                    .field(
                        "retryAfterMs",
                        reply.retry_after.map(|d| d.as_millis() as u64),
                    )
                    .emit();
            }
            status => {
                log::warn(log_ids::AGENT_HTTP_FAILED)
                    .field("route", route)
                    .field("status", status)
                    .emit();
            }
        }
    }

    fn network_failed(route: &str) {
        log::warn(log_ids::AGENT_HTTP_FAILED)
            .field("route", route)
            .field("kind", "network")
            .emit();
    }

    /// `GET /mm/v1/official/hello`: the clock offset, the grant ring and the activities.
    fn hello(&mut self) -> bool {
        let t0 = local_ms();
        let reply = self.client.call("GET", HELLO, None, None, self.seconds());
        let t1 = local_ms();
        let reply = match reply {
            Ok(r) if r.status == 200 => r,
            Ok(r) => {
                self.refused("hello", &r);
                return false;
            }
            Err(()) => {
                Self::network_failed("hello");
                return false;
            }
        };
        let parsed = serde_json::from_slice::<Value>(&reply.body)
            .ok()
            .and_then(|v| {
                let time = v.get("serverTimeMs")?.as_i64()?;
                let keys = v.get("grantVerifyKeys")?.as_array()?;
                let ring = keys
                    .iter()
                    .map(|k| {
                        Some(format!(
                            "{}:{}",
                            k.get("kid")?.as_str()?,
                            k.get("publicKey")?.as_str()?
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?
                    .join(",");
                KeyRing::parse(&ring).ok()?;
                let activities = v
                    .get("activities")?
                    .as_array()?
                    .iter()
                    .filter_map(|a| a.as_str().map(String::from))
                    .collect::<Vec<_>>();
                Some((time, ring, keys.len(), activities))
            });
        let Some((time, ring, keys, activities)) = parsed else {
            log::warn(log_ids::AGENT_HELLO_FAILED)
                .field("reason", "body")
                .emit();
            return false;
        };
        self.offset_ms = time - (t0 + t1) / 2;
        self.ring = ring;
        self.hello_activities = activities;
        log::info(log_ids::AGENT_HELLO)
            .field("keys", keys)
            .field("activities", json!(self.hello_activities))
            .emit();
        self.update_activities();
        true
    }

    /// The activities placed: the override or the hello's, without Road Trip when no catalog
    /// event is in (the engine would refuse that world's only lobby).
    fn update_activities(&mut self) {
        let base: Vec<&'static str> = match &self.cfg.activities {
            Some(a) => a.clone(),
            None => config::ACTIVITIES
                .iter()
                .copied()
                .filter(|a| self.hello_activities.iter().any(|h| h == a))
                .collect(),
        };
        let effective: Vec<&'static str> = base
            .into_iter()
            .filter(|a| *a != "roadtrip" || self.road_trip.is_some())
            .collect();
        if effective != self.activities {
            self.activities = effective;
            log::info(log_ids::AGENT_ACTIVITIES)
                .field("activities", json!(self.activities))
                .field("modes", json!(self.cfg.modes))
                .field("roadTrip", self.road_trip.is_some())
                .emit();
        }
    }

    fn stopped(&self) -> i32 {
        log::info(log_ids::AGENT_STOPPED).emit();
        exit::OK
    }

    fn supervise(&mut self, tx: &Sender<Input>, rx: &Receiver<Input>) -> i32 {
        let mut generation = 0u64;
        let mut delay = Duration::from_secs(1);
        loop {
            generation += 1;
            match self.reconcile_stale(generation, tx) {
                Outcome::Busy => {
                    log::error(log_ids::AGENT_ENGINE_REFUSED)
                        .field("op", "hello")
                        .field("code", "busy")
                        .emit();
                    return exit::ENGINE_BUSY;
                }
                Outcome::Stop => return self.stopped(),
                Outcome::Ended => {}
            }
            generation += 1;
            let started = Instant::now();
            let cap = self.cfg.max_worlds;
            let seats = cap * usize::from(self.cfg.max_players);
            let mut child =
                match engine::spawn(&self.cfg.bind.to_string(), &self.cfg.control, cap, seats) {
                    Ok(c) => c,
                    Err(_) => {
                        log::error(log_ids::AGENT_ENGINE_EXITED)
                            .field("kind", "spawn")
                            .field("restartInMs", delay.as_millis() as u64)
                            .emit();
                        if wait_signal(rx, delay) {
                            return self.stopped();
                        }
                        delay = (delay * 2).min(BACKOFF_MAX);
                        continue;
                    }
                };
            log::info(log_ids::AGENT_ENGINE_STARTED)
                .field("pid", child.id())
                .emit();
            let control = Control::connect(
                &self.cfg.control,
                Instant::now() + Duration::from_secs(5),
                &mut || matches!(child.try_wait(), Ok(None)),
                generation,
                tx.clone(),
            );
            let outcome = match &control {
                Some(c) => self.drive(c, &mut child, generation, rx),
                None => Outcome::Ended,
            };
            if matches!(outcome, Outcome::Stop) {
                self.stop(control.as_ref(), &mut child);
                return self.stopped();
            }
            if let Some(c) = &control {
                c.shutdown_write();
            }
            // the engine ended: every world it served is gone (§2.3 process rule)
            let code = end_child(&mut child, Duration::from_secs(3));
            for w in std::mem::take(&mut self.worlds) {
                self.schedule_delete(w.id);
            }
            self.run_deletions(true);
            if started.elapsed() >= STABLE_RUN {
                delay = Duration::from_secs(1);
            }
            log::warn(log_ids::AGENT_ENGINE_EXITED)
                .field("code", code)
                .field("restartInMs", delay.as_millis() as u64)
                .emit();
            if wait_signal(rx, delay) {
                self.run_deletions(true);
                return self.stopped();
            }
            delay = (delay * 2).min(BACKOFF_MAX);
        }
    }

    /// An engine already answering on the control socket (one an earlier agent left behind):
    /// its worlds are `DELETE`d on the server and it is shut down, so this agent starts clean.
    fn reconcile_stale(&mut self, generation: u64, tx: &Sender<Input>) -> Outcome {
        if !self.cfg.control.exists() {
            return Outcome::Ended;
        }
        let Some(c) = Control::connect(
            &self.cfg.control,
            Instant::now(),
            &mut || false,
            generation,
            tx.clone(),
        ) else {
            return Outcome::Ended;
        };
        let hello = c.request(
            "hello",
            json!({"proto": crate::PROTO_MAX, "controller": {"kind": "vm", "version": crate::VERSION}}),
        );
        let worlds = match hello {
            Ok(m) => ids(m.get("worlds")),
            Err(e) if e.code == "busy" => return Outcome::Busy,
            Err(_) => {
                c.shutdown_write();
                return Outcome::Ended;
            }
        };
        log::warn(log_ids::AGENT_ENGINE_STALE)
            .field("worlds", worlds.len())
            .emit();
        for id in worlds {
            self.schedule_delete(id);
        }
        self.run_deletions(true);
        let _ = c.request("shutdown", json!({}));
        c.shutdown_write();
        // wait until it stops answering (it exits after its shutdown)
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until
            && std::os::unix::net::UnixStream::connect(&self.cfg.control).is_ok()
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        Outcome::Ended
    }

    fn drive(
        &mut self,
        control: &Control,
        child: &mut Child,
        generation: u64,
        rx: &Receiver<Input>,
    ) -> Outcome {
        let hello = control.request(
            "hello",
            json!({"proto": crate::PROTO_MAX, "controller": {"kind": "vm", "version": crate::VERSION}}),
        );
        let hello = match hello {
            Ok(m) => m,
            Err(e) => {
                self.engine_refused("hello", &e);
                return if e.code == "busy" {
                    Outcome::Busy
                } else {
                    Outcome::Ended
                };
            }
        };
        self.engine_cap = hello
            .get("capacity")
            .and_then(|c| c.get("maxWorlds"))
            .and_then(Value::as_u64)
            .map_or(self.cfg.max_worlds, |n| {
                (n as usize).min(self.cfg.max_worlds)
            });
        // a fresh child has none; any listed is closed and deleted, never adopted
        for id in ids(hello.get("worlds")) {
            let _ = control.request_retry(
                "world.close",
                json!({"worldId": id, "reason": "controller"}),
            );
            self.schedule_delete(id);
        }
        if let Err(e) = control.request_retry("keys.set", json!({"ring": self.ring})) {
            self.engine_refused("keys.set", &e);
            return Outcome::Ended;
        }
        if let Some(set) = &self.road_trip
            && let Err(e) = control.request_retry("catalog.set", Value::Object(set.clone()))
        {
            self.engine_refused("catalog.set", &e);
        }
        self.engine_full = false;
        self.generation = generation;
        self.sessions.clear();
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Input::Signal) | Err(RecvTimeoutError::Disconnected) => return Outcome::Stop,
                Ok(Input::Closed(g)) if g == generation => return Outcome::Ended,
                Ok(Input::Event(g, ev)) if g == generation => self.on_event(control, &ev),
                Ok(Input::GameSession(g, answer)) if g == generation => {
                    self.session_answered(control, answer)
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
            if !matches!(child.try_wait(), Ok(None)) {
                return Outcome::Ended;
            }
            self.tick(control);
        }
    }

    fn engine_refused(&self, op: &str, e: &engine::CmdError) {
        log::warn(log_ids::AGENT_ENGINE_REFUSED)
            .field("op", op.to_string())
            .field("code", e.code.clone())
            .field("reason", e.reason.clone())
            .emit();
    }

    fn on_event(&mut self, control: &Control, ev: &Value) {
        let world_id = ev.get("worldId").and_then(Value::as_str);
        match ev.get("ev").and_then(Value::as_str) {
            Some("world.state") => {
                let Some(w) = self
                    .worlds
                    .iter_mut()
                    .find(|w| Some(w.id.as_str()) == world_id)
                else {
                    return;
                };
                if ev.get("running").and_then(Value::as_bool) == Some(false) {
                    return;
                }
                let mut peers = ids(ev.get("peers"));
                peers.dedup();
                peers.truncate(12);
                let grants = ev
                    .get("admission")
                    .and_then(|a| a.get("grants"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let handshaking = ev.get("handshaking").and_then(Value::as_u64).unwrap_or(0);
                w.peers = peers;
                w.seen_peers |= !w.peers.is_empty();
                let in_progress = ev.get("inProgress").and_then(Value::as_bool) == Some(true);
                if in_progress != w.in_progress {
                    // announced at once: placement stops (or starts again) sending newcomers
                    w.in_progress = in_progress;
                    w.next_announce = Instant::now();
                }
                w.busy = grants > 0 || handshaking > 0;
                if w.peers.is_empty() && !w.busy {
                    w.idle_since.get_or_insert_with(Instant::now);
                } else {
                    w.idle_since = None;
                }
            }
            Some("world.closed") => {
                let Some(i) = self
                    .worlds
                    .iter()
                    .position(|w| Some(w.id.as_str()) == world_id)
                else {
                    return;
                };
                let w = self.worlds.remove(i);
                log::warn(log_ids::AGENT_WORLD_ENDED)
                    .world(&w.id)
                    .field(
                        "reason",
                        ev.get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("other")
                            .to_string(),
                    )
                    .emit();
                self.freed();
                self.schedule_delete(w.id);
            }
            Some("world.gameSession.request") => self.game_session_request(control, ev),
            Some("catalog.need") => {
                if ev.get("kind").and_then(Value::as_str) != Some("parts") {
                    return;
                }
                let Some(model) = ev
                    .get("model")
                    .and_then(Value::as_u64)
                    .and_then(|m| u32::try_from(m).ok())
                else {
                    return;
                };
                // without a catalog every model is unknown: `slots: {}`, not judged (§9)
                let slots = self
                    .catalog
                    .as_mut()
                    .map(|c| c.parts(model))
                    .unwrap_or_default();
                if let Err(e) = control.request_retry(
                    "catalog.parts",
                    json!({"game": GAME, "model": model, "slots": slots}),
                ) {
                    self.engine_refused("catalog.parts", &e);
                }
            }
            _ => {}
        }
    }

    /// A world closed: the caps may have room again.
    fn freed(&mut self) {
        self.engine_full = false;
        self.server_full_until = None;
        self.capacity_logged = false;
    }

    /// `world.gameSession.request {worldId, rid, kind, xuids}` of one of this agent's car meet
    /// lobbies: the signed `POST …/game-sessions` on a thread of its own (the loop keeps
    /// announcing), answered by [`Agent::session_answered`] or `null` at the 14 s deadline. Any
    /// other request is answered `null` at once, never left hanging (§4.4).
    fn game_session_request(&mut self, control: &Control, ev: &Value) {
        let Some(world) = ev.get("worldId").and_then(Value::as_str).map(String::from) else {
            return;
        };
        let Some(rid) = ev.get("rid").and_then(Value::as_u64) else {
            return;
        };
        let kind = ev.get("kind").and_then(Value::as_str);
        let xuids = ev.get("xuids").and_then(Value::as_array).cloned();
        let ours = self
            .worlds
            .iter()
            .any(|w| w.id == world && w.activity == config::MIXED && !w.group);
        // OD-191: free roam, or Road Trip with a catalog (the group world runs it); Showdown and
        // anything else are not served, answered at once
        let kind = match kind {
            Some("freeroam") => Some("freeroam"),
            Some("roadtrip") if self.road_trip.is_some() => Some("roadtrip"),
            _ => None,
        };
        let refusal = if !ours || kind.is_none() {
            Some("not_served")
        } else if xuids.as_ref().is_none_or(|x| {
            x.is_empty() || x.len() > GAME_SESSION_XUIDS_MAX || x.iter().any(|v| !v.is_string())
        }) {
            Some("invalid")
        } else if self.sessions.len() >= GAME_SESSIONS_MAX {
            Some("busy")
        } else {
            None
        };
        if let Some(code) = refusal {
            self.answer_session(control, &world, rid, Err(code), None);
            return;
        }
        let body = json!({"kind": kind, "xuids": xuids}).to_string();
        let target = format!("{}/game-sessions", Self::world_target(&world));
        let key = uuid_v4();
        let seconds = self.seconds();
        let client = self.session_client.clone();
        let inputs = self.inputs.clone();
        let generation = self.generation;
        let world_id = world.clone();
        let spawned = std::thread::Builder::new()
            .name("agent-game-session".into())
            .spawn(move || {
                let reply =
                    client.call("POST", &target, Some(body.as_bytes()), Some(&key), seconds);
                let (session, status) = match reply {
                    Ok(r) if r.status == 200 || r.status == 201 => {
                        (session_of(&r.body).ok_or("body"), Some(r.status))
                    }
                    // the VM is at `maxWorlds` (a group world counts): not served here now
                    Ok(r)
                        if r.status == 409
                            && error_reason(&r.body).as_deref() == Some("max_worlds") =>
                    {
                        (Err("not_served"), Some(r.status))
                    }
                    Ok(r) => (Err("refused"), Some(r.status)),
                    Err(()) => (Err("network"), None),
                };
                let _ = inputs.send(Input::GameSession(
                    generation,
                    SessionAnswer {
                        world: world_id,
                        rid,
                        session,
                        status,
                    },
                ));
            });
        if spawned.is_err() {
            self.answer_session(control, &world, rid, Err("internal"), None);
            return;
        }
        self.sessions.push(PendingSession {
            world,
            rid,
            deadline: Instant::now() + GAME_SESSION_BUDGET,
        });
    }

    /// A game-session call came back: answered unless its deadline already answered `null`.
    /// A made session's group world is opened on the engine (with its context) **before** the
    /// answer, so the players' games find it when they connect (WORLDS_OFFICIAL.md §11.4).
    fn session_answered(&mut self, control: &Control, a: SessionAnswer) {
        let Some(i) = self
            .sessions
            .iter()
            .position(|p| p.world == a.world && p.rid == a.rid)
        else {
            if let Ok(made) = &a.session {
                // answered null already: the server's group world is not served here
                self.schedule_delete(made.world.clone());
            }
            return;
        };
        self.sessions.remove(i);
        let session = match a.session {
            Ok(made) => match self.open_group(control, &made) {
                Ok(()) => Ok(made),
                Err(code) => {
                    self.schedule_delete(made.world.clone());
                    Err(code)
                }
            },
            Err(code) => Err(code),
        };
        self.answer_session(control, &a.world, a.rid, session, a.status);
    }

    /// `world.open` + `world.context` of a session's group world, then announced like the
    /// others from now on (its first announce at once: the punches admit its players).
    fn open_group(&mut self, control: &Control, made: &MadeSession) -> Result<(), &'static str> {
        if self.worlds.iter().any(|w| w.id == made.world) {
            return Ok(());
        }
        let opened = control.request_retry(
            "world.open",
            json!({"worldId": made.world, "game": GAME,
                "host": {"kind": "official", "vmId": self.cfg.vm_id},
                "activity": made.activity, "maxPlayers": self.cfg.max_players,
                "races": true, "requireProof": true}),
        );
        if let Err(e) = opened {
            self.engine_refused("world.open", &e);
            if e.code == "capacity" {
                self.engine_full = true;
            }
            return Err(if e.code == "capacity" {
                "capacity"
            } else {
                "engine"
            });
        }
        let context = control.request_retry(
            "world.context",
            json!({"worldId": made.world, "serverTimeMs": self.server_ms(),
                "world": {"id": made.world, "game": GAME, "host": {"official": self.cfg.vm_id}}}),
        );
        if let Err(e) = context {
            self.engine_refused("world.context", &e);
            let _ = control.request_retry(
                "world.close",
                json!({"worldId": made.world, "reason": "controller"}),
            );
            return Err("engine");
        }
        let now = Instant::now();
        self.worlds.push(World {
            id: made.world.clone(),
            mode: self.cfg.modes.first().copied().unwrap_or("noob"),
            activity: made.activity,
            peers: Vec::new(),
            busy: false,
            idle_since: Some(now),
            next_announce: now,
            announce_key: uuid_v4(),
            last_admission: None,
            group: true,
            seen_peers: false,
            in_progress: false,
            opened: now,
        });
        log::info(log_ids::AGENT_WORLD_OPENED)
            .world(&made.world)
            .field("activity", made.activity)
            .field("group", true)
            .emit();
        Ok(())
    }

    /// `world.gameSession.answer`: the session, or `session: null` with the refusal's code (the
    /// engine cancels the invitation; a `stale` reply means it already gave up).
    fn answer_session(
        &mut self,
        control: &Control,
        world: &str,
        rid: u64,
        session: Result<MadeSession, &'static str>,
        status: Option<u16>,
    ) {
        let (fields, outcome) = match &session {
            Ok(made) => (
                json!({"worldId": world, "rid": rid,
                    "session": {"name": made.name, "templateName": made.template}}),
                "created",
            ),
            Err(code) => (
                json!({"worldId": world, "rid": rid, "session": null, "error": code}),
                *code,
            ),
        };
        let engine = match control.request_retry("world.gameSession.answer", fields) {
            Ok(_) => "ok".to_string(),
            Err(e) => e.code,
        };
        let mut line = log::info(log_ids::AGENT_GAME_SESSION)
            .world(world)
            .field("outcome", outcome)
            .field("engine", engine);
        if let Some(s) = status {
            line = line.field("status", s);
        }
        line.emit();
    }

    /// The game-session calls past their 14 s budget: `session: null` (the late answer is
    /// dropped when it comes).
    fn expire_sessions(&mut self, control: &Control, now: Instant) {
        let (late, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.sessions)
            .into_iter()
            .partition(|p| p.deadline <= now);
        self.sessions = keep;
        for p in late {
            self.answer_session(control, &p.world, p.rid, Err("timeout"), None);
        }
    }

    fn tick(&mut self, control: &Control) {
        let now = Instant::now();
        self.expire_sessions(control, now);
        if now >= self.next_hello {
            let ring = self.ring.clone();
            if self.hello() {
                self.next_hello = now + HELLO_REFRESH;
                if self.ring != ring
                    && let Err(e) = control.request_retry("keys.set", json!({"ring": self.ring}))
                {
                    self.engine_refused("keys.set", &e);
                }
            } else {
                self.next_hello = now + Duration::from_secs(30);
            }
        }
        self.run_deletions(false);
        let due: Vec<String> = self
            .worlds
            .iter()
            .filter(|w| w.next_announce <= now)
            .map(|w| w.id.clone())
            .collect();
        for id in due {
            self.announce(control, &id);
        }
        self.place(control, now);
    }

    fn announce(&mut self, control: &Control, id: &str) {
        let Some(w) = self.worlds.iter().find(|w| w.id == id) else {
            return;
        };
        let mut body = json!({"peers": w.peers, "activity": w.activity});
        if w.in_progress {
            body["inProgress"] = Value::Bool(true);
        }
        let body = body.to_string();
        let key = w.announce_key.clone();
        let target = format!("{}/announce", Self::world_target(id));
        let reply = self.client.call(
            "POST",
            &target,
            Some(body.as_bytes()),
            Some(&key),
            self.seconds(),
        );
        let now = Instant::now();
        let interval = Duration::from_secs(self.cfg.announce_seconds);
        let next = match reply {
            Ok(r) if r.status == 200 => {
                if let Some(w) = self.worlds.iter_mut().find(|w| w.id == id) {
                    w.announce_key = uuid_v4();
                    w.next_announce = now + interval;
                }
                self.apply(control, id, &r.body);
                return;
            }
            Ok(r) if r.status == 404 || r.status == 403 => {
                // lapsed, closed or not ours: the engine's copy goes too; placement opens a new id
                log::warn(log_ids::AGENT_WORLD_LOST)
                    .world(id)
                    .field("status", r.status)
                    .emit();
                self.worlds.retain(|w| w.id != id);
                let _ = control.request_retry(
                    "world.close",
                    json!({"worldId": id, "reason": "controller"}),
                );
                self.freed();
                return;
            }
            Ok(r) => {
                self.refused("announce", &r);
                if r.status == 401 {
                    self.next_hello = now;
                }
                now + r
                    .retry_after
                    .filter(|_| r.status == 429)
                    .unwrap_or(ANNOUNCE_RETRY)
            }
            Err(()) => {
                Self::network_failed("announce");
                now + ANNOUNCE_RETRY
            }
        };
        if let Some(w) = self.worlds.iter_mut().find(|w| w.id == id) {
            w.next_announce = next;
        }
    }

    /// A `PUT`/announce answer (`WorldAnnounced`): `world.context` (the lease), then
    /// `world.admission` with its punches (§4.3).
    fn apply(&mut self, control: &Control, id: &str, body: &[u8]) {
        let answer: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let mut punches = answer
            .get("punches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        punches.truncate(PUNCHES_MAX);
        let time = self.server_ms();
        // the official host is this VM's own id, never copied from the answer (§6.3)
        let context = control.request_retry(
            "world.context",
            json!({"worldId": id, "serverTimeMs": time,
                "world": {"id": id, "game": GAME, "host": {"official": self.cfg.vm_id}}}),
        );
        if let Err(e) = context {
            self.engine_refused("world.context", &e);
            match e.code.as_str() {
                "no_keys" => self.next_hello = Instant::now(),
                "not_found" | "context_invalid" => self.close(control, id, "context"),
                _ => {}
            }
            return;
        }
        match control.request_retry(
            "world.admission",
            json!({"worldId": id, "serverTimeMs": time, "punches": punches}),
        ) {
            Ok(m) => {
                let admitted = m.get("admitted").and_then(Value::as_u64).unwrap_or(0);
                let now = (punches.len(), admitted);
                if let Some(w) = self.worlds.iter_mut().find(|w| w.id == id)
                    && w.last_admission != Some(now)
                {
                    w.last_admission = Some(now);
                    log::info(log_ids::AGENT_WORLD_ADMISSION)
                        .world(id)
                        .field("punches", punches.len())
                        .field("admitted", admitted)
                        .emit();
                }
            }
            Err(e) => self.engine_refused("world.admission", &e),
        }
    }

    /// One spare open world per (mode, activity), at most the cap; an idle extra closed.
    fn place(&mut self, control: &Control, now: Instant) {
        if now < self.open_after || self.ring.is_empty() {
            return;
        }
        let max = usize::from(self.cfg.max_players);
        // a group world closes once it emptied again, or nobody came within the seats' hold
        let ended = self.worlds.iter().find(|w| {
            w.group
                && idle_for(w, now) >= IDLE_CLOSE
                && (w.seen_peers || now.saturating_duration_since(w.opened) >= GROUP_HOLD)
        });
        if let Some(id) = ended.map(|w| w.id.clone()) {
            self.close(control, &id, "group_ended");
            return;
        }
        // worlds of an activity or mode no longer placed close once idle
        let stray = self.worlds.iter().find(|w| {
            !w.group
                && (!self.activities.contains(&w.activity) || !self.cfg.modes.contains(&w.mode))
                && idle_for(w, now) >= IDLE_CLOSE
        });
        if let Some(id) = stray.map(|w| w.id.clone()) {
            self.close(control, &id, "idle");
            return;
        }
        for (mode, activity) in spare_groups(&self.cfg.modes, &self.activities) {
            let group: Vec<&World> = self
                .worlds
                .iter()
                .filter(|w| !w.group && w.mode == mode && w.activity == activity)
                .collect();
            // OD-194: a Road Trip past its vote takes no newcomers: it is no spare
            let free = group
                .iter()
                .filter(|w| w.peers.len() < max && !w.in_progress)
                .count();
            if free == 0 {
                let server_full = self.server_full_until.is_some_and(|t| now < t);
                let reason = if self.worlds.len() >= self.engine_cap.max(1) {
                    Some("worlds")
                } else if self.engine_full {
                    Some("engine")
                } else if server_full {
                    Some("server")
                } else {
                    None
                };
                match reason {
                    Some(reason) => {
                        if !self.capacity_logged {
                            self.capacity_logged = true;
                            log::warn(log_ids::AGENT_CAPACITY)
                                .field("reason", reason)
                                .emit();
                        }
                    }
                    None => {
                        self.open(control, mode, activity);
                        return;
                    }
                }
            } else if free > 1 {
                let extra = group
                    .iter()
                    .rev()
                    .find(|w| w.peers.is_empty() && idle_for(w, now) >= IDLE_CLOSE)
                    .map(|w| w.id.clone());
                if let Some(id) = extra {
                    self.close(control, &id, "idle");
                    return;
                }
            }
        }
    }

    fn open(&mut self, control: &Control, mode: &'static str, activity: &'static str) {
        let id = self
            .minted
            .entry((mode, activity))
            .or_insert_with(uuid_v4)
            .clone();
        // `carmeet` without `carMeetLobby`: the official car meet lobby (both locations and their
        // free roam, DEDICATED_SERVER.md §16 decision 10)
        let opened = control.request_retry(
            "world.open",
            json!({"worldId": id, "game": GAME,
                "host": {"kind": "official", "vmId": self.cfg.vm_id},
                "activity": activity, "maxPlayers": self.cfg.max_players,
                "races": true, "requireProof": true}),
        );
        if let Err(e) = opened {
            self.engine_refused("world.open", &e);
            match e.code.as_str() {
                "capacity" => self.engine_full = true,
                "exists" => {
                    self.minted.remove(&(mode, activity));
                }
                _ => {}
            }
            self.open_failed();
            return;
        }
        let body = json!({"game": GAME, "mode": mode, "activity": activity,
            "maxPlayers": self.cfg.max_players})
        .to_string();
        let reply = self.client.call(
            "PUT",
            &Self::world_target(&id),
            Some(body.as_bytes()),
            None,
            self.seconds(),
        );
        let now = Instant::now();
        match reply {
            Ok(r) if r.status == 200 || r.status == 201 => {
                self.minted.remove(&(mode, activity));
                self.open_failures = 0;
                self.worlds.push(World {
                    id: id.clone(),
                    mode,
                    activity,
                    peers: Vec::new(),
                    busy: false,
                    idle_since: Some(now),
                    next_announce: now + Duration::from_secs(self.cfg.announce_seconds),
                    announce_key: uuid_v4(),
                    last_admission: None,
                    group: false,
                    seen_peers: false,
                    in_progress: false,
                    opened: now,
                });
                log::info(log_ids::AGENT_WORLD_OPENED)
                    .world(&id)
                    .field("mode", mode)
                    .field("activity", activity)
                    .emit();
                self.apply(control, &id, &r.body);
                return;
            }
            Ok(r) => {
                let reason = error_reason(&r.body);
                match r.status {
                    401 => {
                        self.refused("open", &r);
                        self.next_hello = now;
                    }
                    409 if reason.as_deref() == Some("max_worlds") => {
                        self.server_full_until = Some(now + BACKOFF_MAX);
                    }
                    400 | 403 | 409 => {
                        log::warn(log_ids::AGENT_WORLD_REFUSED)
                            .world(&id)
                            .field("status", r.status)
                            .field("reason", reason)
                            .emit();
                        self.minted.remove(&(mode, activity));
                    }
                    _ => self.refused("open", &r),
                }
                if r.status == 429
                    && let Some(d) = r.retry_after
                {
                    self.open_after = now + d;
                } else {
                    self.open_failed();
                }
            }
            Err(()) => {
                Self::network_failed("open");
                self.open_failed();
            }
        }
        // the engine's world goes; a retry opens it again with the same id
        let _ = control.request_retry(
            "world.close",
            json!({"worldId": id, "reason": "controller"}),
        );
    }

    fn open_failed(&mut self) {
        self.open_failures += 1;
        self.open_after = Instant::now() + backoff(self.open_failures);
    }

    /// Closes one of this agent's worlds: the engine's copy, then `DELETE`.
    fn close(&mut self, control: &Control, id: &str, why: &str) {
        self.worlds.retain(|w| w.id != id);
        let _ = control.request_retry(
            "world.close",
            json!({"worldId": id, "reason": "controller"}),
        );
        log::info(log_ids::AGENT_WORLD_CLOSED)
            .world(id)
            .field("why", why.to_string())
            .emit();
        self.freed();
        self.schedule_delete(id.to_string());
        self.run_deletions(false);
    }

    fn schedule_delete(&mut self, id: String) {
        if !self.deletions.iter().any(|d| d.id == id) {
            self.deletions.push(Deletion {
                id,
                next: Instant::now(),
                tries: 0,
            });
        }
    }

    /// The due `DELETE`s (`all`: every one, once, as when stopping).
    fn run_deletions(&mut self, all: bool) {
        let now = Instant::now();
        let mut keep = Vec::new();
        for mut d in std::mem::take(&mut self.deletions) {
            if !all && d.next > now {
                keep.push(d);
                continue;
            }
            let reply = self.client.call(
                "DELETE",
                &Self::world_target(&d.id),
                None,
                None,
                self.seconds(),
            );
            match reply {
                Ok(r) if matches!(r.status, 200 | 204 | 404 | 403) => {
                    log::info(log_ids::AGENT_WORLD_DELETED)
                        .world(&d.id)
                        .field("status", r.status)
                        .emit();
                    continue;
                }
                Ok(r) => {
                    self.refused("delete", &r);
                    d.tries += 1;
                    d.next = now
                        + r.retry_after
                            .filter(|_| r.status == 429)
                            .unwrap_or(backoff(d.tries));
                }
                Err(()) => {
                    Self::network_failed("delete");
                    d.tries += 1;
                    d.next = now + backoff(d.tries);
                }
            }
            if d.tries < DELETE_TRIES {
                keep.push(d);
            }
        }
        self.deletions = keep;
    }

    /// SIGTERM/SIGINT: every world `DELETE`d (no more placements), then the engine's `shutdown`
    /// (it closes its worlds with the game's Close to peers and exits 0).
    fn stop(&mut self, control: Option<&Control>, child: &mut Child) {
        log::info(log_ids::AGENT_SIGNAL).emit();
        for w in std::mem::take(&mut self.worlds) {
            self.schedule_delete(w.id);
        }
        self.run_deletions(true);
        if let Some(c) = control {
            let _ = c.request("shutdown", json!({}));
            c.shutdown_write();
        }
        end_child(child, Duration::from_secs(5));
    }
}

/// The placement groups, each kept with one spare: (mode, activity) for `freeroam` and
/// `roadtrip`, and one `carmeet` group for both modes, stored under the first mode
/// (WORLDS_OFFICIAL.md §11.1: `any_mode`, `mode` = the agent's first mode). Free roam first, the
/// car meet lobby, then Road Trip: the order spares are restored in.
fn spare_groups(
    modes: &[&'static str],
    activities: &[&'static str],
) -> Vec<(&'static str, &'static str)> {
    let mut groups = Vec::new();
    for activity in config::ACTIVITIES {
        if !activities.contains(&activity) {
            continue;
        }
        if activity == config::MIXED {
            groups.extend(modes.first().map(|m| (*m, activity)));
        } else {
            groups.extend(modes.iter().map(|m| (*m, activity)));
        }
    }
    groups
}

/// `GameModeSession {name, templateName, world: {worldId, activity}}` of a `game-sessions`
/// answer (MATCHMAKING_API §3.8; WORLDS_OFFICIAL.md §11.4): the group world's id a UUID and its
/// activity `freeroam` or `roadtrip`.
fn session_of(body: &[u8]) -> Option<MadeSession> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let world = v.get("world")?;
    let id = world.get("worldId")?.as_str()?;
    if !crate::core::grant::uuid(id) {
        return None;
    }
    let activity = match world.get("activity")?.as_str()? {
        "freeroam" => "freeroam",
        "roadtrip" => "roadtrip",
        _ => return None,
    };
    Some(MadeSession {
        name: v.get("name")?.as_str()?.to_string(),
        template: v.get("templateName")?.as_str()?.to_string(),
        world: id.to_string(),
        activity,
    })
}

fn idle_for(w: &World, now: Instant) -> Duration {
    if w.busy || !w.peers.is_empty() {
        return Duration::ZERO;
    }
    w.idle_since
        .map_or(Duration::ZERO, |t| now.saturating_duration_since(t))
}

fn ids(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Waits for the child up to `wait`, then kills it; its exit code (`None`: by a signal).
fn end_child(child: &mut Child, wait: Duration) -> Option<i32> {
    let until = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                return child.wait().ok().and_then(|s| s.code());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_spare_per_mode_for_free_roam_and_road_trip_and_one_car_meet_lobby() {
        let all = spare_groups(&config::MODES, &config::ACTIVITIES);
        assert_eq!(
            all,
            vec![
                ("hardcore", "freeroam"),
                ("noob", "freeroam"),
                ("hardcore", "carmeet"),
                ("hardcore", "roadtrip"),
                ("noob", "roadtrip"),
            ]
        );
        // 2 + 1 + 2 spares fit the default cap with room
        assert!(all.len() < config::DEFAULT_MAX_WORLDS as usize);
        // the car meet lobby is stored under the first mode, whatever the order of the list
        assert_eq!(
            spare_groups(&["noob", "hardcore"], &["carmeet"]),
            vec![("noob", "carmeet")]
        );
        // without a catalog (no roadtrip placed) or with one mode
        assert_eq!(
            spare_groups(&["noob"], &["roadtrip", "freeroam", "carmeet"]),
            vec![
                ("noob", "freeroam"),
                ("noob", "carmeet"),
                ("noob", "roadtrip")
            ]
        );
        assert!(spare_groups(&config::MODES, &[]).is_empty());
    }

    #[test]
    fn a_game_session_answer_needs_its_name_and_template() {
        let body = |world: &str| {
            format!(
                r#"{{"kind":"roadtrip","scid":"788f0100-a4bc-46c3-b19b-d1001a96545b","templateName":"defaulttemplate","name":"fh2inf-invite-a-b","members":["1"]{world}}}"#
            )
        };
        let w = "4f1c2a6e-1b2d-4c3e-8f40-5a6b7c8d9e01";
        let ok = body(&format!(
            r#","world":{{"worldId":"{w}","activity":"roadtrip"}}"#
        ));
        assert_eq!(
            session_of(ok.as_bytes()),
            Some(MadeSession {
                name: "fh2inf-invite-a-b".into(),
                template: "defaulttemplate".into(),
                world: w.into(),
                activity: "roadtrip",
            })
        );
        // OD-191: no group world, a malformed id or another activity is not a session
        for world in [
            String::new(),
            r#","world":{"worldId":"nope","activity":"freeroam"}"#.to_string(),
            format!(r#","world":{{"worldId":"{w}","activity":"carmeet"}}"#),
        ] {
            assert_eq!(session_of(body(&world).as_bytes()), None, "{world}");
        }
        assert_eq!(session_of(br#"{"name":"x"}"#), None);
        assert_eq!(session_of(b"not json"), None);
    }
}

mod signals {
    //! SIGTERM and SIGINT through a self-pipe: the handler only writes one byte
    //! (async-signal-safe); a thread turns it into [`Input::Signal`].
    use std::{
        ffi::{c_int, c_void},
        io::Read,
        os::{fd::IntoRawFd, unix::net::UnixStream},
        sync::{
            atomic::{AtomicI32, Ordering},
            mpsc::Sender,
        },
    };

    use super::Input;

    const SIGINT: c_int = 2;
    const SIGTERM: c_int = 15;
    static FD: AtomicI32 = AtomicI32::new(-1);

    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn signal(sig: c_int, handler: usize) -> usize;
        fn write(fd: c_int, buf: *const c_void, n: usize) -> isize;
    }

    #[allow(unsafe_code)]
    extern "C" fn on_signal(sig: c_int) {
        let fd = FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let b = sig as u8;
            // SAFETY: write(2) is async-signal-safe; the buffer is a live local byte.
            unsafe {
                write(fd, &b as *const u8 as *const c_void, 1);
            }
        }
    }

    #[allow(unsafe_code)]
    pub fn install(tx: Sender<Input>) {
        let Ok((mut rx, wr)) = UnixStream::pair() else {
            return;
        };
        FD.store(wr.into_raw_fd(), Ordering::Relaxed);
        // SAFETY: installs a handler that only does an async-signal-safe write.
        unsafe {
            signal(SIGTERM, on_signal as extern "C" fn(c_int) as usize);
            signal(SIGINT, on_signal as extern "C" fn(c_int) as usize);
        }
        let _ = std::thread::Builder::new()
            .name("agent-signals".into())
            .spawn(move || {
                let mut b = [0u8; 1];
                while rx.read_exact(&mut b).is_ok() {
                    if tx.send(Input::Signal).is_err() {
                        return;
                    }
                }
            });
    }
}
