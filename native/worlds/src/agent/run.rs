// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The agent's supervisor and placement loop (DEDICATED_SERVER.md §16; WORLDS_OFFICIAL.md §4, §5;
//! MATCHMAKING_API §2.9):
//!
//! 1. `GET /mm/v1/official/hello` (retried with backoff): the server's clock and grant keys;
//! 2. the engine child, its controller `hello`, `keys.set`, `catalog.set`; restarted with backoff
//!    when it ends, its worlds `DELETE`d on the server;
//! 3. one spare open world per (mode, activity): `world.open` + `PUT` with a client-minted id kept
//!    for its retries; an announce per world every 15 s with `world.state`'s peers, each answer
//!    feeding `world.context` then `world.admission`; an idle extra world closed (`DELETE`);
//! 4. `catalog.need` answered with `catalog.parts`;
//! 5. SIGTERM/SIGINT: `DELETE` every world, `shutdown` the engine, exit 0.

use std::{
    collections::HashMap,
    process::Child,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};

use super::{
    catalog::{self, Catalog},
    config::{self, AgentConfig, GAME},
    engine::{self, Control, Input},
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
        client: Client::new(&cfg.server.base, &cfg.vm_id, key),
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
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Input::Signal) | Err(RecvTimeoutError::Disconnected) => return Outcome::Stop,
                Ok(Input::Closed(g)) if g == generation => return Outcome::Ended,
                Ok(Input::Event(g, ev)) if g == generation => self.on_event(control, &ev),
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

    fn tick(&mut self, control: &Control) {
        let now = Instant::now();
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
        let body = json!({"peers": w.peers, "activity": w.activity}).to_string();
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
        // worlds of an activity or mode no longer placed close once idle
        let stray = self.worlds.iter().find(|w| {
            (!self.activities.contains(&w.activity) || !self.cfg.modes.contains(&w.mode))
                && idle_for(w, now) >= IDLE_CLOSE
        });
        if let Some(id) = stray.map(|w| w.id.clone()) {
            self.close(control, &id, "idle");
            return;
        }
        let groups: Vec<(&'static str, &'static str)> = self
            .cfg
            .modes
            .iter()
            .flat_map(|m| self.activities.iter().map(move |a| (*m, *a)))
            .collect();
        for (mode, activity) in groups {
            let group: Vec<&World> = self
                .worlds
                .iter()
                .filter(|w| w.mode == mode && w.activity == activity)
                .collect();
            let free = group.iter().filter(|w| w.peers.len() < max).count();
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
