// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The FH2 game-data catalogs (contract §9): the engine reads no game file. The controller
//! (the launcher, which reads the player's own game database; later the VM's world agent) sends
//! the rows the engine uses:
//!
//! - `catalog.set {game, version, roadTrip: {events: [{id, location, mode, minPlayers,
//!   classes}]}}`: the Road Trip catalog, latest-wins, idempotent, at most 1024 events;
//! - `catalog.parts {game, model, slots: {<table>: [row ids]}}`: one car model's upgrade rows
//!   for the watch's parts rule, answered on demand: when the watch first needs a model the
//!   engine emits `catalog.need {game, kind: "parts", model}` (deduplicated, rate-limited per
//!   peer, per world and process-wide); a 64-model LRU keeps the known answers and a separate
//!   negative cache the models the game database does not have, so unknown-model answers never
//!   evict known ones; until an answer arrives that model's parts are unknown (never a flag).

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::Arc,
};

use serde_json::{Map, Value, json};

use super::engine::roadtrip::{RoadTripCatalog, RoadTripEvent};
use crate::{control::proto::CmdResult, core::fields::Obj};

/// `catalog.set`: the most Road Trip events, and classes per event.
pub const MAX_ROAD_TRIP_EVENTS: usize = 1024;
pub const MAX_CLASSES: usize = 8;
/// `catalog.set.version`: 1..64 printable ASCII characters.
pub const MAX_VERSION: usize = 64;
/// The parts rule's models kept (the reference's per-model cache size).
pub const MAX_CACHED_MODELS: usize = 64;
/// `catalog.parts`: the most row ids for one model (every slot), and per slot.
pub const MAX_PART_ROWS: usize = 4096;
pub const MAX_SLOT_ROWS: usize = 1024;
/// A model asked for is not asked again for this long while no answer came.
pub const NEED_RETRY_MS: u64 = 60_000;
/// At most this many `catalog.need` per window, process-wide: only a backstop, sized so every
/// world spending its own budget (`WORLD_NEEDS_PER_WINDOW` x the public profile's 8 worlds)
/// still fits, so one world can never starve the others.
pub const NEEDS_PER_WINDOW: usize = 64;
pub const NEED_WINDOW_MS: u64 = 10_000;
/// At most this many `catalog.need` per window for one world (requester `world`).
pub const WORLD_NEEDS_PER_WINDOW: usize = 8;
/// At most this many new models asked for one peer per `PEER_NEED_WINDOW_MS` (a player who
/// changes car often still gets an answer within the minute; one writing bogus models cannot
/// spend the world's budget).
pub const PEER_NEEDS_PER_WINDOW: usize = 2;
pub const PEER_NEED_WINDOW_MS: u64 = 60_000;
/// Requesters (world, peer) remembered for their budgets, at most (oldest windows go first).
pub const MAX_REQUESTERS: usize = 1024;
/// Models the game database does not have, remembered apart from the known LRU.
pub const MAX_UNKNOWN_MODELS: usize = 256;
/// Models waiting for an answer, at most.
pub const MAX_PENDING_NEEDS: usize = 256;

/// The installed-upgrade slot tables, in the order of the player object's 43 part rows (the
/// reference's `PartsCatalog.SlotTables`): `catalog.parts.slots` names them.
pub const SLOT_TABLES: [&str; 43] = [
    "List_UpgradeEngine",
    "List_UpgradeDrivetrain",
    "List_UpgradeCarBody",
    "List_UpgradeMotor",
    "List_UpgradeBrakes",
    "List_UpgradeSpringDamper",
    "List_UpgradeAntiSwayFront",
    "List_UpgradeAntiSwayRear",
    "List_UpgradeTireCompound",
    "List_UpgradeRearWing",
    "List_UpgradeRimSizeFront",
    "List_UpgradeRimSizeRear",
    "List_UpgradeEngineCamshaft",
    "List_UpgradeEngineValves",
    "List_UpgradeEngineDisplacement",
    "List_UpgradeEnginePistonsCompression",
    "List_UpgradeEngineFuelSystem",
    "List_UpgradeEngineIgnition",
    "List_UpgradeEngineExhaust",
    "List_UpgradeEngineIntake",
    "List_UpgradeEngineFlywheel",
    "List_UpgradeEngineManifold",
    "List_UpgradeEngineRestrictorPlate",
    "List_UpgradeEngineOilCooling",
    "List_UpgradeEngineTurboSingle",
    "List_UpgradeEngineTurboTwin",
    "List_UpgradeEngineTurboQuad",
    "List_UpgradeEngineCSC",
    "List_UpgradeEngineDSC",
    "List_UpgradeEngineIntercooler",
    "List_UpgradeDrivetrainClutch",
    "List_UpgradeDrivetrainTransmission",
    "List_UpgradeDrivetrainDriveline",
    "List_UpgradeDrivetrainDifferential",
    "List_UpgradeCarBodyFrontBumper",
    "List_UpgradeCarBodyRearBumper",
    "List_UpgradeCarBodyHood",
    "List_UpgradeCarBodySideSkirt",
    "List_UpgradeCarBodyTireWidthFront",
    "List_UpgradeCarBodyTireWidthRear",
    "List_UpgradeCarBodyWeight",
    "List_UpgradeCarBodyChassisStiffness",
    "List_UpgradeMotorParts",
];

/// One model's upgrade rows by slot (the rows of each slot's table that belong to the model);
/// an empty answer (no slot) is a model the game database does not have.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelParts {
    pub model: u32,
    pub by_slot: Vec<BTreeSet<u32>>,
    /// The controller found the model (false: a car pack or a placeholder, not judged).
    pub known: bool,
    /// Every slot's rows together, built once when the answer is stored (never per check).
    all: BTreeSet<u32>,
}

impl ModelParts {
    /// One model's rows; `all` is derived here.
    pub fn new(model: u32, by_slot: Vec<BTreeSet<u32>>, known: bool) -> ModelParts {
        let all = by_slot.iter().flatten().copied().collect();
        ModelParts {
            model,
            by_slot,
            known,
            all,
        }
    }

    /// Every row of any slot's table that belongs to this model.
    pub fn all(&self) -> &BTreeSet<u32> {
        &self.all
    }
}

/// Who asks for a model: the world (a process-unique id per watch) and the peer's XUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Requester {
    pub world: u64,
    pub peer: u64,
}

/// What the parts rule gets for a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartsLookup {
    /// The answer came.
    Known(Arc<ModelParts>),
    /// Asked (or about to be): the rule says nothing about this model yet.
    Unknown,
}

/// The process-wide FH2 catalogs.
#[derive(Debug, Default)]
pub struct CatalogStore {
    road_trip: Option<(String, Arc<RoadTripCatalog>)>,
    /// LRU of known models: the front is the least recently used.
    parts: VecDeque<Arc<ModelParts>>,
    /// Negative cache: models the controller answered as absent (front: oldest).
    unknown: VecDeque<Arc<ModelParts>>,
    /// Models asked for and when.
    pending: BTreeMap<u32, u64>,
    window: (u64, usize),
    /// Per world and per (world, peer) need windows: (start, count).
    worlds: HashMap<u64, (u64, usize)>,
    peers: HashMap<Requester, (u64, usize)>,
    /// `catalog.need` events to emit (models).
    needs: Vec<u32>,
}

impl CatalogStore {
    /// The Road Trip catalog, when one with events was set.
    pub fn road_trip(&self) -> Option<Arc<RoadTripCatalog>> {
        self.road_trip
            .as_ref()
            .filter(|(_, c)| !c.is_empty())
            .map(|(_, c)| c.clone())
    }

    /// `catalog.set` → `{changed, events}`.
    pub fn set(&mut self, o: Obj) -> CmdResult {
        let version = o.str("version")?;
        if version.is_empty()
            || version.len() > MAX_VERSION
            || !version.bytes().all(|b| (0x20..=0x7e).contains(&b))
        {
            return Err(o.bad("version"));
        }
        let rt = Obj::new(o.obj("roadTrip")?, "roadTrip");
        let list = rt.arr("events")?;
        if list.len() > MAX_ROAD_TRIP_EVENTS {
            return Err(rt.bad("events"));
        }
        let mut events = Vec::with_capacity(list.len());
        for v in list {
            let Value::Object(m) = v else {
                return Err(rt.bad("events"));
            };
            let e = Obj::new(m, "roadTrip.events");
            let int = |k: &str, max: u64| e.int(k, 0, max).map(|n| n as i32);
            let classes = e.arr("classes")?;
            if classes.len() > MAX_CLASSES {
                return Err(e.bad("classes"));
            }
            let mut set = BTreeSet::new();
            for c in classes {
                set.insert(
                    c.as_u64()
                        .filter(|n| *n <= 7)
                        .ok_or_else(|| e.bad("classes"))? as i32,
                );
            }
            events.push(RoadTripEvent {
                id: int("id", i32::MAX as u64)?,
                location: int("location", 255)?,
                mode: int("mode", 255)?,
                min_players: int("minPlayers", 255)?,
                classes: set,
            });
        }
        let catalog = RoadTripCatalog::new(events);
        let kept = catalog.events.len();
        let changed = self
            .road_trip
            .as_ref()
            .is_none_or(|(v, c)| v != version || **c != catalog);
        if changed {
            self.road_trip = Some((version.to_string(), Arc::new(catalog)));
        }
        let mut m = Map::new();
        m.insert("changed".into(), json!(changed));
        m.insert("events".into(), json!(kept));
        Ok(m)
    }

    /// `catalog.parts` → `{}`: one model's rows, latest-wins per model.
    pub fn set_parts(&mut self, o: Obj) -> CmdResult {
        let model = o.int("model", 0, u64::from(u32::MAX))? as u32;
        let slots = o.obj("slots")?;
        let mut by_slot = vec![BTreeSet::new(); SLOT_TABLES.len()];
        let mut total = 0usize;
        for (table, rows) in slots {
            let Some(slot) = SLOT_TABLES.iter().position(|t| t == table) else {
                return Err(o.bad("slots"));
            };
            let Value::Array(rows) = rows else {
                return Err(o.bad("slots"));
            };
            if rows.len() > MAX_SLOT_ROWS {
                return Err(o.bad("slots"));
            }
            total += rows.len();
            if total > MAX_PART_ROWS {
                return Err(o.bad("slots"));
            }
            for r in rows {
                by_slot[slot].insert(
                    r.as_u64()
                        .filter(|n| *n <= u64::from(u32::MAX))
                        .ok_or_else(|| o.bad("slots"))? as u32,
                );
            }
        }
        let known = !slots.is_empty();
        self.pending.remove(&model);
        self.parts.retain(|p| p.model != model);
        self.unknown.retain(|p| p.model != model);
        let entry = Arc::new(ModelParts::new(model, by_slot, known));
        // an absent model goes to the negative cache: it never evicts a known one
        let (cache, cap) = if known {
            (&mut self.parts, MAX_CACHED_MODELS)
        } else {
            (&mut self.unknown, MAX_UNKNOWN_MODELS)
        };
        if cache.len() >= cap {
            cache.pop_front();
        }
        cache.push_back(entry);
        Ok(Map::new())
    }

    /// The parts rule's rows for a model at `now` (the process's monotonic ms), asked by no
    /// particular world or peer (tests, tools): see [`CatalogStore::parts_for`].
    pub fn parts(&mut self, model: u32, now: u64) -> PartsLookup {
        self.parts_for(model, now, Requester::default())
    }

    /// The parts rule's rows for a model at `now`: known, or unknown with a `catalog.need`
    /// queued (deduplicated; rate-limited per peer, per world, then process-wide).
    pub fn parts_for(&mut self, model: u32, now: u64, who: Requester) -> PartsLookup {
        if let Some(i) = self.parts.iter().position(|p| p.model == model) {
            let p = self.parts.remove(i).expect("found");
            self.parts.push_back(p.clone());
            return PartsLookup::Known(p);
        }
        if let Some(p) = self.unknown.iter().find(|p| p.model == model) {
            return PartsLookup::Known(p.clone());
        }
        if self
            .pending
            .get(&model)
            .is_some_and(|at| now.saturating_sub(*at) < NEED_RETRY_MS)
        {
            return PartsLookup::Unknown;
        }
        // the cheapest refusals first: the peer's, then the world's, then the process's
        let open = |w: &(u64, usize), span: u64, cap: usize| {
            now.saturating_sub(w.0) >= span || w.1 < cap
        };
        if self.peers.get(&who).is_some_and(|w| {
            !open(w, PEER_NEED_WINDOW_MS, PEER_NEEDS_PER_WINDOW)
        }) || self.worlds.get(&who.world).is_some_and(|w| {
            !open(w, NEED_WINDOW_MS, WORLD_NEEDS_PER_WINDOW)
        }) {
            return PartsLookup::Unknown;
        }
        if now.saturating_sub(self.window.0) >= NEED_WINDOW_MS {
            self.window = (now, 0);
        }
        if self.window.1 >= NEEDS_PER_WINDOW {
            return PartsLookup::Unknown;
        }
        self.pending
            .retain(|_, at| now.saturating_sub(*at) < NEED_RETRY_MS);
        if self.pending.len() >= MAX_PENDING_NEEDS {
            return PartsLookup::Unknown;
        }
        if self.peers.len() >= MAX_REQUESTERS || self.worlds.len() >= MAX_REQUESTERS {
            self.peers
                .retain(|_, w| now.saturating_sub(w.0) < PEER_NEED_WINDOW_MS);
            self.worlds
                .retain(|_, w| now.saturating_sub(w.0) < NEED_WINDOW_MS);
            if self.peers.len() >= MAX_REQUESTERS || self.worlds.len() >= MAX_REQUESTERS {
                return PartsLookup::Unknown;
            }
        }
        let charge = |w: &mut (u64, usize), span: u64| {
            if now.saturating_sub(w.0) >= span {
                *w = (now, 0);
            }
            w.1 += 1;
        };
        charge(self.peers.entry(who).or_insert((now, 0)), PEER_NEED_WINDOW_MS);
        charge(self.worlds.entry(who.world).or_insert((now, 0)), NEED_WINDOW_MS);
        self.window.1 += 1;
        self.pending.insert(model, now);
        self.needs.push(model);
        PartsLookup::Unknown
    }

    /// The `catalog.need` models queued since the last call.
    pub fn take_needs(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.needs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_are_asked_once_and_kept_in_a_64_model_lru() {
        let mut s = CatalogStore::default();
        assert_eq!(s.parts(7, 0), PartsLookup::Unknown);
        assert_eq!(s.parts(7, 10), PartsLookup::Unknown);
        assert_eq!(s.take_needs(), vec![7]);
        let body = json!({"model": 7, "slots": {"List_UpgradeBrakes": [3, 4]}});
        let Value::Object(m) = body else { panic!() };
        s.set_parts(Obj::new(&m, "")).unwrap();
        let PartsLookup::Known(p) = s.parts(7, 20) else {
            panic!()
        };
        assert!(p.known && p.by_slot[4].contains(&4));
        for model in 100..170 {
            let b = json!({"model": model, "slots": {}});
            let Value::Object(m) = b else { panic!() };
            s.set_parts(Obj::new(&m, "")).unwrap();
        }
        // absent models live in the negative cache: the known model 7 is still kept
        assert_eq!(s.parts.len(), 1);
        assert!(matches!(s.parts(7, 30), PartsLookup::Known(_)));
        for model in 200..270 {
            let b = json!({"model": model, "slots": {"List_UpgradeBrakes": [1]}});
            let Value::Object(m) = b else { panic!() };
            s.set_parts(Obj::new(&m, "")).unwrap();
        }
        assert_eq!(s.parts.len(), MAX_CACHED_MODELS);
        assert_eq!(s.parts(7, 40), PartsLookup::Unknown);
        let PartsLookup::Known(absent) = s.parts(150, 50) else {
            panic!()
        };
        assert!(!absent.known);
    }

    #[test]
    fn needs_are_rate_limited() {
        let mut s = CatalogStore::default();
        for m in 0..200 {
            let who = Requester {
                world: u64::from(m / 10),
                peer: u64::from(m),
            };
            s.parts_for(m, 0, who);
        }
        assert_eq!(s.take_needs().len(), NEEDS_PER_WINDOW);
        s.parts_for(999, NEED_WINDOW_MS, Requester { world: 77, peer: 1 });
        assert_eq!(s.take_needs(), vec![999]);
    }

    #[test]
    fn one_peer_cannot_starve_the_process_wide_parts_need_window() {
        let mut s = CatalogStore::default();
        let bad = Requester { world: 1, peer: 9 };
        for m in 0..1000 {
            s.parts_for(m, m.into(), bad);
        }
        assert_eq!(s.take_needs().len(), PEER_NEEDS_PER_WINDOW);
        let good = Requester { world: 2, peer: 5 };
        s.parts_for(5000, 1000, good);
        assert_eq!(s.take_needs(), vec![5000]);
    }
}
