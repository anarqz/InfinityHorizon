// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! Events to the controller (contract §4.5, §4.6): a bounded queue (1024 lines or 1 MiB) the loop
//! never blocks on. `world.state` is coalesced latest-wins per world and always delivered last;
//! `watch.report` keeps 64 per world; `peer.rejected` and `diag` are dropped first. Every drop is
//! counted and reported by an `events.dropped` event before the next event. `seq` is assigned
//! when an event leaves the queue (1, 2, ... per controller session), so the sequence the
//! controller sees has no gap a drop did not explain. Events made while no controller is
//! attached are discarded (a reconnecting controller reconciles from `hello.worlds`).

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Map, Value, json};

pub const MAX_LINES: usize = 1024;
pub const MAX_BYTES: usize = 1024 * 1024;
pub const WATCH_PER_WORLD: usize = 64;

// event ids (contract §4.5)
pub const WORLD_STATE: &str = "world.state";
pub const PEER_SYNCED: &str = "peer.synced";
pub const PEER_LEFT: &str = "peer.left";
pub const WORLD_EMPTY: &str = "world.empty";
pub const WORLD_OCCUPIED: &str = "world.occupied";
pub const WORLD_CLOSED: &str = "world.closed";
pub const GAME_SESSION_REQUEST: &str = "world.gameSession.request";
pub const WATCH_REPORT: &str = "watch.report";
pub const PEER_REJECTED: &str = "peer.rejected";
pub const DIAG: &str = "diag";
pub const NOTICE: &str = "notice";
pub const DS_DRAINING: &str = "ds.draining";
pub const EVENTS_DROPPED: &str = "events.dropped";

// notice ids (contract §4.7)
pub const N_NO_ROADTRIP: &str = "ds.world.no_roadtrip";
pub const N_NO_PARTS: &str = "ds.world.no_parts";
pub const N_ADMISSION_LAPSED: &str = "ds.admission.lapsed";
pub const N_ADMISSION_CLEARED: &str = "ds.admission.cleared";
pub const N_CAPACITY_REFUSED: &str = "ds.capacity.refused";
pub const N_CPU_GUARD: &str = "ds.cpu.guard";
pub const N_MEMORY_HIGH: &str = "ds.memory.high";
pub const N_SESSION_CANCELLED: &str = "ds.carmeet.session_cancelled";
pub const N_CONTROL_STALLED: &str = "ds.control.stalled";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// `world.state`: coalesced, never dropped.
    State,
    /// `watch.report`: 64 per world.
    Watch,
    /// `peer.rejected`, `diag`: dropped first.
    Low,
    Normal,
    /// `world.closed`, `world.gameSession.request`, `ds.draining`: never dropped.
    Critical,
}

fn class_of(ev: &str) -> Class {
    match ev {
        WORLD_STATE => Class::State,
        WATCH_REPORT => Class::Watch,
        PEER_REJECTED | DIAG => Class::Low,
        WORLD_CLOSED | GAME_SESSION_REQUEST | DS_DRAINING => Class::Critical,
        _ => Class::Normal,
    }
}

struct Item {
    ev: &'static str,
    world: Option<String>,
    class: Class,
    body: Map<String, Value>,
    size: usize,
}

#[derive(Default)]
pub struct EventQueue {
    items: VecDeque<Item>,
    bytes: usize,
    enabled: bool,
    seq: u64,
    dropped: BTreeMap<String, u64>,
    /// Totals for `stats`.
    pub dropped_total: u64,
    /// `watch.report`s dropped (each one is also logged by the caller).
    pub watch_dropped: u64,
}

impl EventQueue {
    pub fn new() -> EventQueue {
        EventQueue::default()
    }

    /// A controller session started (`enabled`) or ended: the queue and `seq` start over.
    pub fn reset(&mut self, enabled: bool) {
        self.items.clear();
        self.bytes = 0;
        self.seq = 0;
        self.dropped.clear();
        self.enabled = enabled;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.dropped.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.items.len() >= MAX_LINES || self.bytes >= MAX_BYTES
    }

    fn count_drop(&mut self, ev: &str) {
        *self.dropped.entry(ev.to_string()).or_default() += 1;
        self.dropped_total += 1;
        if ev == WATCH_REPORT {
            self.watch_dropped += 1;
        }
    }

    fn remove(&mut self, i: usize) -> Option<Item> {
        let item = self.items.remove(i)?;
        self.bytes -= item.size;
        Some(item)
    }

    /// Queues an event. Returns false when it (or an older one) was dropped.
    pub fn push(
        &mut self,
        ev: &'static str,
        world: Option<&str>,
        body: Map<String, Value>,
    ) -> bool {
        if !self.enabled {
            return true;
        }
        let class = class_of(ev);
        let size = Value::Object(body.clone()).to_string().len() + ev.len() + 32;
        let mut clean = true;
        if class == Class::State
            && let Some(i) = self
                .items
                .iter()
                .position(|it| it.class == Class::State && it.world.as_deref() == world)
        {
            self.remove(i);
        }
        if class == Class::Watch {
            let same: Vec<usize> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, it)| it.class == Class::Watch && it.world.as_deref() == world)
                .map(|(i, _)| i)
                .collect();
            if same.len() >= WATCH_PER_WORLD {
                self.remove(same[0]);
                self.count_drop(WATCH_REPORT);
                clean = false;
            }
        }
        while self.items.len() + 1 > MAX_LINES || self.bytes + size > MAX_BYTES {
            let victim = [Class::Low, Class::Normal, Class::Watch]
                .iter()
                .find_map(|c| self.items.iter().position(|it| it.class == *c));
            match victim {
                Some(i) => {
                    if let Some(it) = self.remove(i) {
                        self.count_drop(it.ev);
                    }
                    clean = false;
                }
                None if matches!(class, Class::Low | Class::Normal | Class::Watch) => {
                    self.count_drop(ev);
                    return false;
                }
                // only State and Critical left: they go past the cap (a handful per world)
                None => break,
            }
        }
        self.bytes += size;
        self.items.push_back(Item {
            ev,
            world: world.map(str::to_string),
            class,
            body,
            size,
        });
        clean
    }

    /// The next line to send (with its `seq`), `events.dropped` first when anything was dropped.
    pub fn pop_line(&mut self) -> Option<String> {
        let value = if !self.dropped.is_empty() {
            let kinds: Map<String, Value> = std::mem::take(&mut self.dropped)
                .into_iter()
                .map(|(k, v)| (k, json!(v)))
                .collect();
            let count: u64 = kinds.values().filter_map(Value::as_u64).sum();
            self.seq += 1;
            json!({"ev": EVENTS_DROPPED, "seq": self.seq, "count": count, "kinds": kinds})
        } else {
            let item = self.items.pop_front()?;
            self.bytes -= item.size;
            self.seq += 1;
            let mut m = Map::new();
            m.insert("ev".into(), Value::String(item.ev.into()));
            m.insert("seq".into(), json!(self.seq));
            for (k, v) in item.body {
                if k != "ev" && k != "seq" {
                    m.insert(k, v);
                }
            }
            Value::Object(m)
        };
        let mut line = value.to_string();
        line.push('\n');
        Some(line)
    }
}
