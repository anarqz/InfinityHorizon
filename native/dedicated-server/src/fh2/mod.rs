// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The Forza Horizon 2 game module (`game: "fh2"`, OD-186): the NerdCore codec (`codec`,
//! written from NERDCORE_WIRE.md, OD-187), the side channels (`carstate`; the retired livery channel is dropped), the engines (free roam,
//! car meets 3 and 6, Road Trip), the FH2 settings (`RelaySettings`), voice, the anti-cheat
//! watch and the game-data catalog (`gamedbRC.slt`).
//!
//! World-scoped FH2 fields of `world.open`: `races`, `voice`, `watch`, `settings`.
//!
//! Step 3: the free-roam engine with its events and races (contract §7), behind the world's
//! admission (core::admission, through [`AdmissionPolicy`]): nobody is admitted before the first
//! `world.context`, and peers only by a verified grant (README, PORT_STATUS).

pub mod carstate;
pub mod catalog;
pub mod codec;
pub mod engine;
pub mod settings;
pub mod voice;

use std::{
    collections::{BTreeMap, HashMap},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use serde_json::{Map, Value, json};

use crate::{
    control::proto::{self, CmdError, CmdResult},
    core::{
        Profile, events,
        fields::{Obj, parse_xuid},
        game::{Admit, Game, GameCtx, GameEvent, GameWorld, Notice, OpenInfo, Send},
        rng::Rng,
    },
};
use catalog::CatalogStore;
use engine::{
    Engine, EngineKind, EngineSettings, Event, Policy,
    random::EngineRandom,
    watch::{WatchReport, WatchRules},
};
pub use settings::RelaySettings;
pub use voice::VoiceMode;

pub const GAME_ID: &str = "fh2";

/// `world.voice.lobbyMuted`, `world.voiceBlocks.blocks`.
pub const VOICE_LIST_MAX: usize = 64;
/// JSON numbers the controller reads exactly (2^53).
const JSON_INT_MAX: u64 = (1 << 53) - 1;
/// The retired livery side channel's STUN message type (contract §5.3: dropped).
pub const LIVERY_KIND: u16 = 0x3E92;

// FH2 notices (contract §4.7)
pub const N_NO_ROADTRIP: &str = "ds.world.no_roadtrip";
pub const N_NO_PARTS: &str = "ds.world.no_parts";

/// The deprecated `--game-db` (contract §9): the engine reads no game file; the catalogs come
/// through the control channel (`catalog.set`, `catalog.parts`). Kept so a launcher that still
/// passes the flag starts; the path is ignored.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub path: Option<PathBuf>,
}

/// The FH2 module: the process-wide catalogs its worlds share.
#[derive(Debug, Default)]
pub struct Fh2 {
    pub catalog: Catalog,
    pub store: Arc<Mutex<CatalogStore>>,
}

impl Fh2 {
    pub fn new(catalog: Catalog) -> Fh2 {
        Fh2 {
            catalog,
            store: Arc::default(),
        }
    }
}

fn lock(store: &Mutex<CatalogStore>) -> std::sync::MutexGuard<'_, CatalogStore> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The engines' policy: the world's admission, all three questions installed (the C#
/// `SetAdmissionPolicyAsync` with a validator, a hook validator and `AwaitsProof`), and the
/// world's voice blocks (compared with the wall clock, Unix seconds).
pub struct AdmissionPolicy(pub Arc<dyn Admit>, pub Arc<Mutex<VoiceBlockTable>>);

impl Policy for AdmissionPolicy {
    fn has_authorize(&self) -> bool {
        true
    }
    fn has_authorize_hook(&self) -> bool {
        true
    }
    fn authorize(&mut self, xuid: u64, from: SocketAddr, now: i64) -> bool {
        self.0.allowed(xuid, from, 0, now.max(0) as u64)
    }
    fn authorize_hook(&mut self, xuid: u64, from: SocketAddr, now: i64) -> bool {
        self.0.allowed(xuid, from, 1, now.max(0) as u64)
    }
    fn awaits_proof(&mut self, xuid: u64, now: i64) -> Option<bool> {
        Some(self.0.proof_expected(xuid, now.max(0) as u64))
    }
    fn voice_blocked(&mut self, xuid: u64, _now: i64) -> bool {
        let unix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        self.1.lock().is_ok_and(|t| t.blocked(xuid, unix))
    }
    /// A world always has its controller (a controller that leaves closes its worlds); a
    /// dedicated world's controller answers `session: null` itself (contract §4.4).
    fn can_create_game_session(&self) -> bool {
        true
    }
}

/// Where a voice block reached the world from (CR-S67).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum VoiceBlockSource {
    Grant,
    Roster,
}

/// The world's voice blocks by XUID (Unix seconds, 0: none), from two sources, "whichever the
/// relay saw last" (contract §6.4): a value a source changed becomes the block; a source
/// repeating itself never overrides the other's newer word; an XUID a source stops listing keeps
/// its block (the other source or the instant ends it), but that source's next word about it
/// counts as new.
#[derive(Debug, Default)]
pub struct VoiceBlockTable {
    blocks: BTreeMap<u64, i64>,
    seen: BTreeMap<(VoiceBlockSource, u64), i64>,
}

impl VoiceBlockTable {
    /// What `source` says now of each XUID; true when a block changed.
    pub fn apply(&mut self, source: VoiceBlockSource, blocks: &BTreeMap<u64, i64>) -> bool {
        let mut changed = false;
        for (&xuid, &until) in blocks {
            let value = until.max(0);
            if self.seen.get(&(source, xuid)) == Some(&value) {
                continue;
            }
            self.seen.insert((source, xuid), value);
            if self.blocks.get(&xuid).copied().unwrap_or(0) == value {
                continue;
            }
            if value == 0 {
                self.blocks.remove(&xuid);
            } else {
                self.blocks.insert(xuid, value);
            }
            changed = true;
        }
        self.seen
            .retain(|(s, x), _| *s != source || blocks.contains_key(x));
        changed
    }

    /// Whether `xuid`'s voice is blocked at `unix` (the block's instant still ahead).
    pub fn blocked(&self, xuid: u64, unix: i64) -> bool {
        self.blocks.get(&xuid).is_some_and(|until| *until > unix)
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

#[derive(Debug, Clone, PartialEq)]
pub struct Watch {
    /// MATCHMAKING_API §3.9 `WatchRules`, verbatim.
    pub rules: Value,
    /// What the engines measure against (`PeerWatchRules.From(rules, forceSpeed)`).
    pub engine_rules: WatchRules,
    pub self_xuid: Option<u64>,
    pub parts: bool,
    pub force_speed: bool,
}

fn xuid_list(o: Obj, key: &str, max: usize) -> Result<Vec<u64>, CmdError> {
    let mut out = Vec::new();
    for v in o.arr(key)? {
        out.push(v.as_str().and_then(parse_xuid).ok_or_else(|| o.bad(key))?);
    }
    if out.len() > max {
        return Err(o.bad(key));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// `world.voice` (and `world.open.voice`): strict mode (the C# maps unknown to open; §14).
pub fn parse_voice(o: Obj) -> Result<Voice, CmdError> {
    let mode = VoiceMode::parse(o.str("mode")?).ok_or_else(|| o.bad("mode"))?;
    let lobby_muted = if o.has("lobbyMuted") {
        xuid_list(o, "lobbyMuted", VOICE_LIST_MAX)?
    } else {
        Vec::new()
    };
    Ok(Voice { mode, lobby_muted })
}

/// `world.watch` (and `world.open.watch`); `Ok(None)` is `rules: null` (stop).
pub fn parse_watch(o: Obj, profile: Profile, is_player: bool) -> Result<Option<Watch>, CmdError> {
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
    let Some(rules) = rules else {
        return Ok(None);
    };
    let engine_rules = watch_rules(&rules, force_speed).ok_or_else(|| o.bad("rules"))?;
    Ok(Some(Watch {
        rules,
        engine_rules,
        self_xuid,
        parts,
        force_speed,
    }))
}

/// MATCHMAKING_API §3.9 `WatchRules` → the engines' rules, as `PeerWatchRules.From`; `None`
/// when a field is missing or the rules are not usable (`WatchRules.Valid`).
pub fn watch_rules(rules: &Value, force_speed: bool) -> Option<WatchRules> {
    let f = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64);
    let u = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let (speed, climb, jump) = (rules.get("speed")?, rules.get("climb")?, rules.get("jump")?);
    let floor = match speed.get("perCarFloorKmh") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_f64()?),
    };
    let r = WatchRules {
        version: u(rules, "version")?,
        speed_max_kmh: f(speed, "maxKmh")?,
        speed_sustain_ms: u(speed, "sustainMs")?,
        climb_max_kmh: f(climb, "maxKmh")?,
        climb_sustain_ms: u(climb, "sustainMs")?,
        jump_max_meters: f(jump, "maxMeters")?,
        force_speed,
        speed_floor_kmh: floor,
    };
    let limit =
        |kmh: f64, sustain: u32| kmh > 0.0 && kmh <= 5000.0 && (1..=3_600_000).contains(&sustain);
    let valid = r.version > 0
        && limit(r.speed_max_kmh, r.speed_sustain_ms)
        && limit(r.climb_max_kmh, r.climb_sustain_ms)
        && r.jump_max_meters > 0.0
        && r.jump_max_meters <= 1_000_000.0
        && r.speed_floor_kmh
            .is_none_or(|fl| fl > 0.0 && fl <= 5000.0 && fl <= r.speed_max_kmh);
    valid.then_some(r)
}

/// A report number as the reference's JSON writes it: an integral value without a fraction.
fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

impl Game for Fh2 {
    fn id(&self) -> &'static str {
        GAME_ID
    }

    fn command(&self, op: &str, o: Obj) -> Option<CmdResult> {
        match op {
            "catalog.set" => Some(lock(&self.store).set(o)),
            "catalog.parts" => Some(lock(&self.store).set_parts(o)),
            _ => None,
        }
    }

    fn open(&self, info: OpenInfo) -> Result<(Box<dyn GameWorld>, Vec<Notice>), CmdError> {
        let o = info.request;
        let races = o.bool("races")?;
        let voice = if o.has("voice") {
            parse_voice(Obj::new(o.obj("voice")?, "voice"))?
        } else {
            Voice::default()
        };
        let watch = if o.has("watch") {
            parse_watch(
                Obj::new(o.obj("watch")?, "watch"),
                info.profile,
                info.is_player,
            )?
        } else {
            None
        };
        // OD-186 item 4: one activity per public world; refused in private
        let activity = match (info.profile, o.has("activity")) {
            (Profile::Private, true) => return Err(o.bad("activity")),
            (Profile::Private, false) => None,
            (Profile::Public, _) => Some(match o.str("activity")? {
                "freeroam" => EngineKind::FreeRoam,
                "roadtrip" => EngineKind::RoadTrip,
                "carmeet" => match o.int("carMeetLobby", 3, 6)? {
                    3 => EngineKind::CarMeet3,
                    6 => EngineKind::CarMeet6,
                    _ => return Err(o.bad("carMeetLobby")),
                },
                _ => return Err(o.bad("activity")),
            }),
        };
        if o.has("carMeetLobby")
            && !matches!(activity, Some(EngineKind::CarMeet3 | EngineKind::CarMeet6))
        {
            return Err(o.bad("carMeetLobby"));
        }
        let mut settings = RelaySettings::for_profile(info.profile);
        if o.has("settings") {
            settings.apply(o.obj("settings")?)?;
        }
        settings.fit_join_snapshot(usize::from(info.max_players));
        let base = EngineSettings {
            relay: settings.clone(),
            max_players: usize::from(info.max_players),
            races,
            car_meet_lobby: None,
            enforce_car_rule: false,
            road_trip: None,
        };
        let mut world = Fh2World {
            is_player: info.is_player,
            races,
            voice,
            watch,
            settings,
            base,
            admit: info.admit.clone(),
            voice_blocks: Arc::new(Mutex::new(VoiceBlockTable::default())),
            rng: Rng::new(info.rng.next_u64()),
            store: self.store.clone(),
            activity,
            engines: BTreeMap::new(),
            routes: HashMap::new(),
            road_trip_noted: false,
            sessions: BTreeMap::new(),
            next_rid: 1,
            now: 0,
            dropped: 0,
            last_activity: std::cell::Cell::new(None),
            events: Vec::new(),
        };
        let base_kind = world.base_kind();
        // the base engine exists from the open (a Road Trip one once a catalog is in)
        if world.engine(base_kind).is_none() && base_kind != EngineKind::RoadTrip {
            return Err(CmdError::bad("settings"));
        }
        let mut notices = Vec::new();
        if world.road_trip_catalog().is_none() && activity.is_none_or(|a| a == EngineKind::RoadTrip)
        {
            notices.push(Notice {
                id: N_NO_ROADTRIP,
                level: "info",
                args: None,
            });
        }
        Ok((Box::new(world), notices))
    }
}

/// The car meet session requests a world keeps waiting for their answer.
const MAX_SESSIONS: usize = 16;

/// One FH2 world's game side: its engines (free roam, the car meets, Road Trip; contract §7)
/// and the routing of a host socket between them.
pub struct Fh2World {
    is_player: bool,
    races: bool,
    voice: Voice,
    watch: Option<Watch>,
    settings: RelaySettings,
    /// The engines' common settings (each kind adds its own).
    base: EngineSettings,
    admit: Arc<dyn Admit>,
    /// The seeds of the engines made later.
    rng: Rng,
    store: Arc<Mutex<CatalogStore>>,
    /// A public world's one activity (`None`: private, every lobby type).
    activity: Option<EngineKind>,
    engines: BTreeMap<EngineKind, Engine>,
    /// Where an endpoint's datagrams go once it was admitted outside the base engine.
    routes: HashMap<SocketAddr, EngineKind>,
    /// Road Trip asked for with no catalog: noted once.
    road_trip_noted: bool,
    /// `world.gameSession.request` ids (`rid`) → (engine, its request id).
    sessions: BTreeMap<u32, (EngineKind, u32)>,
    next_rid: u32,
    now: u64,
    /// The voice blocks of both sources (§6.4), shared with every engine's policy.
    voice_blocks: Arc<Mutex<VoiceBlockTable>>,
    dropped: u64,
    /// OD-150: the activity the host's own game was last seen in.
    last_activity: std::cell::Cell<Option<&'static str>>,
    /// Events for the core (peer.synced, peer.left, peer.rejected, requests, notices).
    events: Vec<GameEvent>,
}

impl Fh2World {
    /// The engine every other lobby type goes to: free roam, or a public world's activity.
    fn base_kind(&self) -> EngineKind {
        self.activity.unwrap_or(EngineKind::FreeRoam)
    }

    fn road_trip_catalog(&self) -> Option<Arc<engine::roadtrip::RoadTripCatalog>> {
        lock(&self.store).road_trip()
    }

    /// The engine of `kind`, made on first use (a Road Trip one only with a catalog).
    fn engine(&mut self, kind: EngineKind) -> Option<&mut Engine> {
        if !self.engines.contains_key(&kind) {
            let mut s = self.base.clone();
            match kind {
                EngineKind::FreeRoam => {}
                EngineKind::CarMeet3 => s.car_meet_lobby = Some(3),
                EngineKind::CarMeet6 => s.car_meet_lobby = Some(6),
                EngineKind::RoadTrip => {
                    let catalog = self.road_trip_catalog()?;
                    s.road_trip = Some(engine::roadtrip::RoadTripSettings {
                        catalog,
                        storm_island: false,
                        seed: self.rng.seed_i32(),
                    });
                }
            }
            let rng = &mut self.rng;
            let random = EngineRandom::from_source(|| rng.next_u64());
            let mut e = Engine::new(
                kind,
                s,
                random,
                Box::new(AdmissionPolicy(
                    self.admit.clone(),
                    self.voice_blocks.clone(),
                )),
            )?;
            // the world's voice and watch apply to every engine, made now or later
            e.set_voice(self.voice.mode, &self.voice.lobby_muted);
            if self.watch.is_some() {
                self.apply_watch(&mut e);
            }
            self.engines.insert(kind, e);
        }
        self.engines.get_mut(&kind)
    }

    fn notice(&mut self, id: &'static str, args: Option<Value>) {
        let mut body = Map::new();
        body.insert("id".into(), json!(id));
        body.insert("level".into(), json!("info"));
        if let Some(a) = args {
            body.insert("args".into(), a);
        }
        self.events.push(GameEvent::Custom {
            ev: events::NOTICE,
            world: true,
            body,
        });
    }

    /// The engine an OpenConnection for `lobby` goes to; `None`: this world does not serve it.
    fn target_of(&mut self, lobby: u32) -> Option<EngineKind> {
        let want = match lobby {
            3 => EngineKind::CarMeet3,
            6 => EngineKind::CarMeet6,
            1 => {
                if self.road_trip_catalog().is_some() {
                    EngineKind::RoadTrip
                } else {
                    if !self.road_trip_noted {
                        self.road_trip_noted = true;
                        crate::log::info(crate::log_ids::WORLD_NOTE)
                            .field("engine", "roadtrip")
                            .field("note", "roadtrip.no_catalog")
                            .emit();
                        self.notice(N_NO_ROADTRIP, None);
                    }
                    // the reference engine's free roam takes it (private); a public world refuses
                    if self.activity == Some(EngineKind::RoadTrip) {
                        return None;
                    }
                    EngineKind::FreeRoam
                }
            }
            _ => EngineKind::FreeRoam,
        };
        match self.activity {
            Some(a) if a != want => None,
            _ => Some(want),
        }
    }

    /// The endpoint is seated in `target` (a datagram, or a held open taken in after its proof):
    /// its later datagrams follow it there, and its seat in the engine it left goes.
    fn moved(&mut self, target: EngineKind, peer: SocketAddr, now: u64) {
        let base = self.base_kind();
        let current = self.routes.get(&peer).copied().unwrap_or(base);
        if target == current || !self.engines.get(&target).is_some_and(|e| e.has_peer(peer)) {
            return;
        }
        if let Some(e) = self.engines.get_mut(&current) {
            e.forget(peer, now as i64);
        }
        if target == base {
            self.routes.remove(&peer);
        } else {
            self.routes.insert(peer, target);
        }
    }

    fn route(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>) {
        let from = crate::stun::canonical(from);
        let base = self.base_kind();
        if crate::stun::is_stun(data) {
            // hook side channels name their player: the engine holding the endpoint, else every
            // engine (only the one seating it acts)
            let kinds: Vec<EngineKind> = match self.routes.get(&from) {
                Some(k) => vec![*k],
                None => self.engines.keys().copied().collect(),
            };
            for k in kinds {
                if let Some(e) = self.engines.get_mut(&k) {
                    e.receive(data, from, now as i64);
                    out.extend(e.take_out());
                }
            }
            return;
        }
        let current = self.routes.get(&from).copied().unwrap_or(base);
        let Some(lobby) = Engine::lobby_type_of(data) else {
            match self.engines.get_mut(&current) {
                Some(e) => {
                    e.receive(data, from, now as i64);
                    out.extend(e.take_out());
                }
                None => self.dropped += 1,
            }
            return;
        };
        let target = self.target_of(lobby);
        let Some(target) = target.filter(|t| self.engine(*t).is_some()) else {
            // not served here: the activity's engine answers refusal 4
            match self.engines.get_mut(&current) {
                Some(e) => {
                    e.refuse_open(data, from, now as i64);
                    out.extend(e.take_out());
                }
                None => self.dropped += 1,
            }
            return;
        };
        if let Some(e) = self.engines.get_mut(&target) {
            e.receive(data, from, now as i64);
            out.extend(e.take_out());
        }
        self.moved(target, from, now);
    }

    /// The engines' world-log lines go to the process log (indexes and ids only, never an
    /// identity or an address); the other events feed `world.state`, which is read fresh.
    fn drain_events(&mut self) {
        let mut moves = Vec::new();
        let mut requests = Vec::new();
        let mut reports = Vec::new();
        for (kind, e) in self.engines.iter_mut() {
            let engine = kind.as_str();
            for ev in e.take_events() {
                match ev {
                    Event::Note { id, args } => {
                        if id == "carmeet.cancelled" {
                            requests.push((*kind, None, args.first().cloned()));
                        }
                        if crate::log::enabled(crate::log::Level::Info) {
                            crate::log::info(crate::log_ids::WORLD_NOTE)
                                .field("engine", engine)
                                .field("note", id)
                                .field("args", Value::Array(args))
                                .emit();
                        }
                    }
                    Event::ConnectionOpened {
                        index,
                        a,
                        b,
                        refusal,
                    } => {
                        let mut l = crate::log::info(crate::log_ids::WORLD_OPEN_CONNECTION)
                            .field("engine", engine)
                            .field("peer", index)
                            .field("a", a)
                            .field("b", b);
                        if let Some(r) = refusal {
                            l = l.field("refusal", r);
                        }
                        l.emit();
                    }
                    Event::ValidationRejected {
                        xuid,
                        kind,
                        message,
                        router,
                        value,
                    } => self.events.push(GameEvent::Rejected {
                        xuid,
                        kind,
                        message,
                        router,
                        value,
                    }),
                    Event::PeerSynced { xuid, peer } => {
                        self.events
                            .push(GameEvent::PeerSynced { xuid, engine, peer })
                    }
                    Event::PeerLeft {
                        xuid,
                        peer,
                        why,
                        synced,
                    } => {
                        if synced {
                            self.events.push(GameEvent::PeerLeft {
                                xuid,
                                engine,
                                peer,
                                reason: why.left_reason(),
                            });
                        }
                    }
                    Event::AdmittedLate { endpoint } => moves.push((*kind, endpoint)),
                    Event::WatchReport(r) => reports.push(r),
                    Event::CreateGameSession { id, kind: k, xuids } => {
                        requests.push((*kind, Some((id, k, xuids)), None));
                    }
                }
            }
        }
        for r in reports {
            self.watch_report(r);
        }
        for (kind, request, why) in requests {
            match request {
                Some((id, _invite, xuids)) => {
                    let rid = self.next_rid;
                    self.next_rid = self.next_rid.wrapping_add(1).max(1);
                    if self.sessions.len() >= MAX_SESSIONS
                        && let Some(k) = self.sessions.keys().next().copied()
                    {
                        self.sessions.remove(&k);
                    }
                    self.sessions.insert(rid, (kind, id));
                    let xuids: Vec<String> = xuids.iter().map(u64::to_string).collect();
                    let mut body = Map::new();
                    body.insert("rid".into(), json!(rid));
                    body.insert("kind".into(), json!("freeroam"));
                    body.insert("xuids".into(), json!(xuids));
                    self.events.push(GameEvent::Custom {
                        ev: events::GAME_SESSION_REQUEST,
                        world: true,
                        body,
                    });
                }
                None => {
                    self.notice(
                        events::N_SESSION_CANCELLED,
                        Some(json!({"why": why.unwrap_or(Value::Null)})),
                    );
                }
            }
        }
        let now = self.now;
        for (kind, endpoint) in moves {
            self.moved(kind, endpoint, now);
        }
        for model in lock(&self.store).take_needs() {
            let mut body = Map::new();
            body.insert("game".into(), json!(GAME_ID));
            body.insert("kind".into(), json!("parts"));
            body.insert("model".into(), json!(model));
            self.events.push(GameEvent::Custom {
                ev: "catalog.need",
                world: false,
                body,
            });
        }
    }

    fn voice_blocks(&mut self, o: Obj) -> CmdResult {
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
            next.insert(x, until as i64);
        }
        let changed = self.apply_voice_blocks(VoiceBlockSource::Roster, &next);
        let mut m = Map::new();
        m.insert("changed".into(), Value::Bool(changed));
        Ok(m)
    }

    /// One source's word on the voice blocks; a change reaches every engine at once (their
    /// state downs go out with the engine's next datagrams). True when a block changed.
    fn apply_voice_blocks(
        &mut self,
        source: VoiceBlockSource,
        blocks: &BTreeMap<u64, i64>,
    ) -> bool {
        let changed = self
            .voice_blocks
            .lock()
            .is_ok_and(|mut t| t.apply(source, blocks));
        if changed {
            for e in self.engines.values_mut() {
                e.voice_blocks_changed();
            }
        }
        changed
    }

    fn watch_cmd(&mut self, o: Obj, ctx: &mut GameCtx) -> CmdResult {
        let w = parse_watch(o, ctx.profile, self.is_player)?;
        self.watch = w;
        let mut engines = std::mem::take(&mut self.engines);
        for e in engines.values_mut() {
            self.apply_watch(e);
        }
        self.engines = engines;
        self.drain_events();
        Ok(Map::new())
    }

    /// The world's watch on one engine: its rules, the host's own XUID, and the catalog store
    /// for the parts rule when asked for (`None`: stop).
    fn apply_watch(&self, e: &mut Engine) {
        let now = self.now as i64;
        match &self.watch {
            Some(w) => e.set_watch(
                Some(w.engine_rules.clone()),
                w.self_xuid.unwrap_or(0),
                true,
                w.parts.then(|| self.store.clone()),
                now,
            ),
            None => e.set_watch(None, 0, false, None, now),
        }
    }

    /// `watch.report` (contract §4.5): an id minted here, the report's fields.
    fn watch_report(&mut self, r: WatchReport) {
        let mut body = Map::new();
        body.insert("id".into(), json!(self.rng.uuid()));
        body.insert("xuid".into(), json!(r.xuid.to_string()));
        body.insert("kind".into(), json!(r.kind));
        body.insert("value".into(), number(r.value));
        if let Some(s) = r.sustain_ms {
            body.insert("sustainMs".into(), json!(s));
        }
        body.insert("rulesVersion".into(), json!(r.rules_version));
        body.insert("peer".into(), json!(r.peer));
        if let Some(c) = r.car_id {
            body.insert("carId".into(), json!(c));
        }
        if let Some(h) = r.held_kmh {
            body.insert("heldKmh".into(), number(h));
        }
        self.events.push(GameEvent::Custom {
            ev: events::WATCH_REPORT,
            world: true,
            body,
        });
    }

    /// `world.gameSession.answer` (§4.4): the session goes to the invitation that asked;
    /// `stale` when it is not being created any more.
    fn game_session_answer(&mut self, o: Obj, now: u64) -> CmdResult {
        let rid = o.int("rid", 0, u64::from(u32::MAX))? as u32;
        let session = match o.map.get("session") {
            Some(Value::Null) => None,
            Some(Value::Object(s)) => {
                let s = Obj::new(s, "session");
                Some((s.str("name")?, s.str("templateName")?))
            }
            _ => return Err(o.bad("session")),
        };
        if o.has("error") {
            o.str("error")?;
        }
        let Some((kind, id)) = self.sessions.remove(&rid) else {
            return Err(CmdError::code(proto::STALE));
        };
        self.now = self.now.max(now);
        let Some(e) = self.engines.get_mut(&kind) else {
            return Err(CmdError::code(proto::STALE));
        };
        let taken =
            e.game_session_created(id, session.map(|s| s.0), session.map(|s| s.1), now as i64);
        self.drain_events();
        if taken {
            Ok(Map::new())
        } else {
            Err(CmdError::code(proto::STALE))
        }
    }
}

impl GameWorld for Fh2World {
    fn spec(&self) -> Value {
        json!({
            "races": self.races,
            "activity": self.activity.map(EngineKind::as_str),
            "voice": {"mode": self.voice.mode.as_str(), "lobbyMuted": self.voice.lobby_muted},
            "watch": self.watch.as_ref().map(|w| json!({
                "rules": w.rules, "self": w.self_xuid, "parts": w.parts, "forceSpeed": w.force_speed,
            })),
            "settings": format!("{:?}", self.settings),
        })
    }

    fn lobbies(&self) -> Vec<&'static str> {
        let road_trip = self.road_trip_catalog().is_some();
        match self.activity {
            None if road_trip => vec!["carmeet", "roadtrip"],
            None => vec!["carmeet"],
            Some(EngineKind::FreeRoam) => Vec::new(),
            Some(EngineKind::RoadTrip) => vec!["roadtrip"],
            Some(_) => vec!["carmeet"],
        }
    }

    fn command(&mut self, op: &str, o: Obj, ctx: &mut GameCtx) -> Option<CmdResult> {
        Some(match op {
            "world.voice" => parse_voice(o).map(|v| {
                for e in self.engines.values_mut() {
                    e.set_voice(v.mode, &v.lobby_muted);
                }
                self.voice = v;
                Map::new()
            }),
            "world.voiceBlocks" => self.voice_blocks(o),
            "world.watch" => self.watch_cmd(o, ctx),
            "world.gameSession.answer" => self.game_session_answer(o, ctx.now),
            _ => return None,
        })
    }

    fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>) {
        if crate::stun::is_stun(data) && u16::from_be_bytes([data[0], data[1]]) == LIVERY_KIND {
            self.dropped += 1;
            return;
        }
        self.now = self.now.max(now);
        self.route(data, from, now, out);
        self.drain_events();
    }

    fn tick(&mut self, now: u64, out: &mut Vec<Send>) {
        self.now = self.now.max(now);
        for e in self.engines.values_mut().filter(|e| !e.is_idle()) {
            e.tick(now as i64);
            out.extend(e.take_out());
        }
        self.drain_events();
    }

    fn next_tick(&self, profile: Profile, now: u64) -> Option<u64> {
        if self.engines.values().all(Engine::is_idle) {
            None
        } else if self.synced() > 0 || profile == Profile::Private {
            Some(now + 20)
        } else {
            Some(now + 100)
        }
    }

    fn synced(&self) -> usize {
        self.engines.values().map(Engine::synced).sum()
    }

    fn handshaking(&self) -> usize {
        self.engines.values().map(Engine::handshaking).sum()
    }

    fn kick(&mut self, xuid: u64, now: u64, out: &mut Vec<Send>) -> usize {
        let mut n = 0;
        for e in self.engines.values_mut() {
            n += e.kick(xuid, now as i64);
            out.extend(e.take_out());
        }
        self.drain_events();
        n
    }

    fn close_all(&mut self, out: &mut Vec<Send>) {
        for e in self.engines.values_mut() {
            e.close_all();
            out.extend(e.take_out());
        }
        self.drain_events();
    }

    fn state(&self) -> Map<String, Value> {
        let mut engines = Map::new();
        for (k, e) in &self.engines {
            engines.insert(k.as_str().into(), json!(e.synced()));
        }
        let lobby: usize = self
            .engines
            .iter()
            .filter(|(k, _)| **k != EngineKind::FreeRoam)
            .map(|(_, e)| e.synced())
            .sum();
        let mut peers: Vec<String> = Vec::new();
        for e in self.engines.values() {
            for x in e.peer_xuids() {
                let x = x.to_string();
                if !peers.contains(&x) {
                    peers.push(x);
                }
            }
        }
        let mut m = Map::new();
        m.insert("peers".into(), json!(peers));
        m.insert("lobbyPlayers".into(), json!(lobby));
        // OD-150: the engine the host's own (loopback) game synced in last: Road Trip or free
        // roam; a car meet keeps the last value; a public world says its own activity
        let latest = self
            .engines
            .iter()
            .filter_map(|(k, e)| e.loopback_synced_at().map(|at| (at, *k)))
            .max();
        let host_in = self.is_player && latest.is_some();
        if host_in {
            match latest.map(|l| l.1) {
                Some(EngineKind::FreeRoam) => self.last_activity.set(Some("freeroam")),
                Some(EngineKind::RoadTrip) => self.last_activity.set(Some("roadtrip")),
                _ => {}
            }
        }
        let activity = match self.activity {
            Some(EngineKind::FreeRoam) => json!("freeroam"),
            Some(EngineKind::RoadTrip) => json!("roadtrip"),
            Some(_) => json!("carmeet"),
            None => self.last_activity.get().map_or(Value::Null, |a| json!(a)),
        };
        m.insert("activity".into(), activity);
        m.insert("engines".into(), Value::Object(engines));
        m.insert("hostInWorld".into(), Value::Bool(host_in));
        m
    }

    fn dropped(&self) -> u64 {
        self.dropped + self.engines.values().map(|e| e.dropped).sum::<u64>()
    }

    fn occupants(&self, except: u64) -> usize {
        self.engines.values().map(|e| e.occupants(except)).sum()
    }

    fn take_events(&mut self) -> Vec<GameEvent> {
        std::mem::take(&mut self.events)
    }

    fn set_grant_voice_blocks(&mut self, blocks: &BTreeMap<u64, i64>) {
        self.apply_voice_blocks(VoiceBlockSource::Grant, blocks);
    }
}
