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
pub mod codec;
pub mod engine;
pub mod settings;
pub mod voice;

use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use serde_json::{Map, Value, json};

use crate::{
    control::proto::{self, CmdError, CmdResult},
    core::{
        Profile,
        fields::{Obj, parse_xuid},
        game::{Admit, Game, GameCtx, GameEvent, GameWorld, Notice, OpenInfo, Send},
    },
};
use engine::{Engine, EngineKind, EngineSettings, Event, Policy, random::EngineRandom};
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

/// What the FH2 catalog gave at start (contract §9). Step 1 loads none: the read-only SQLite
/// reader and the Road Trip and parts tables come with those features.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub path: Option<PathBuf>,
    pub road_trip_events: bool,
    pub parts: bool,
}

/// The FH2 module.
#[derive(Debug, Default)]
pub struct Fh2 {
    pub catalog: Catalog,
}

impl Fh2 {
    pub fn new(catalog: Catalog) -> Fh2 {
        Fh2 { catalog }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    /// MATCHMAKING_API §3.9 `WatchRules`, verbatim.
    pub rules: Value,
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
    Ok(rules.map(|rules| Watch {
        rules,
        self_xuid,
        parts,
        force_speed,
    }))
}

impl Game for Fh2 {
    fn id(&self) -> &'static str {
        GAME_ID
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
        let mut settings = RelaySettings::for_profile(info.profile);
        if o.has("settings") {
            settings.apply(o.obj("settings")?)?;
        }
        let max_players = usize::from(info.max_players);
        let engine_settings = EngineSettings {
            relay: settings.clone(),
            max_players,
            races,
            car_meet_lobby: None,
            enforce_car_rule: false,
        };
        let rng = &mut *info.rng;
        let random = EngineRandom::from_source(|| rng.next_u64());
        let voice_blocks = Arc::new(Mutex::new(VoiceBlockTable::default()));
        let mut free_roam = Engine::new(
            EngineKind::FreeRoam,
            engine_settings,
            random,
            Box::new(AdmissionPolicy(info.admit.clone(), voice_blocks.clone())),
        )
        .ok_or_else(|| CmdError::bad("settings"))?;
        free_roam.set_voice(voice.mode, &voice.lobby_muted);
        let mut engines = BTreeMap::new();
        engines.insert(EngineKind::FreeRoam, free_roam);
        let mut notices = Vec::new();
        if !self.catalog.road_trip_events {
            notices.push(Notice {
                id: N_NO_ROADTRIP,
                level: "info",
                args: None,
            });
        }
        if watch.as_ref().is_some_and(|w| w.parts) && !self.catalog.parts {
            notices.push(Notice {
                id: N_NO_PARTS,
                level: "info",
                args: None,
            });
        }
        let world = Fh2World {
            is_player: info.is_player,
            races,
            voice,
            watch,
            settings,
            engines,
            catalog: self.catalog.clone(),
            voice_blocks,
            dropped: 0,
            host_seen: std::cell::Cell::new(false),
            events: Vec::new(),
        };
        Ok((Box::new(world), notices))
    }
}

/// One FH2 world's game side.
pub struct Fh2World {
    is_player: bool,
    races: bool,
    voice: Voice,
    watch: Option<Watch>,
    settings: RelaySettings,
    engines: BTreeMap<EngineKind, Engine>,
    catalog: Catalog,
    /// The voice blocks of both sources (§6.4), shared with every engine's policy.
    voice_blocks: Arc<Mutex<VoiceBlockTable>>,
    dropped: u64,
    /// The host's own game synced in an engine at least once (`activity` stays set after).
    host_seen: std::cell::Cell<bool>,
    /// Events for the core (peer.synced, peer.left, peer.rejected).
    events: Vec<GameEvent>,
}

impl Fh2World {
    /// The engines' world-log lines go to the process log (indexes and ids only, never an
    /// identity or an address); the other events feed `world.state`, which is read fresh.
    fn drain_events(&mut self) {
        for (kind, e) in self.engines.iter_mut() {
            let engine = kind.as_str();
            for ev in e.take_events() {
                match ev {
                    Event::Note { id, args } => {
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
                    Event::AdmittedLate { .. } => {}
                }
            }
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
        if w.as_ref().is_some_and(|w| w.parts) && !self.catalog.parts {
            ctx.notices.push(Notice {
                id: N_NO_PARTS,
                level: "info",
                args: None,
            });
        }
        self.watch = w;
        Ok(Map::new())
    }
}

/// `world.gameSession.answer` (§4.4): validated; no invitation is pending in step 1: `stale`.
fn game_session_answer(o: Obj) -> CmdResult {
    o.int("rid", 0, u32::MAX as u64)?;
    match o.map.get("session") {
        Some(Value::Null) => {}
        Some(Value::Object(s)) => {
            let s = Obj::new(s, "session");
            s.str("name")?;
            s.str("templateName")?;
        }
        _ => return Err(o.bad("session")),
    }
    if o.has("error") {
        o.str("error")?;
    }
    Err(CmdError::code(proto::STALE))
}

impl GameWorld for Fh2World {
    fn spec(&self) -> Value {
        json!({
            "races": self.races,
            "voice": {"mode": self.voice.mode.as_str(), "lobbyMuted": self.voice.lobby_muted},
            "watch": self.watch.as_ref().map(|w| json!({
                "rules": w.rules, "self": w.self_xuid, "parts": w.parts, "forceSpeed": w.force_speed,
            })),
            "settings": format!("{:?}", self.settings),
        })
    }

    fn lobbies(&self) -> Vec<&'static str> {
        let mut l = vec!["carmeet"];
        if self.catalog.road_trip_events {
            l.push("roadtrip");
        }
        l
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
            "world.gameSession.answer" => game_session_answer(o),
            _ => return None,
        })
    }

    fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>) {
        if crate::stun::is_stun(data) && u16::from_be_bytes([data[0], data[1]]) == LIVERY_KIND {
            self.dropped += 1;
            return;
        }
        // routing by lobby type (contract §5.1) comes with the car meet and Road Trip engines
        match self.engines.get_mut(&EngineKind::FreeRoam) {
            Some(e) => {
                e.receive(data, from, now as i64);
                out.extend(e.take_out());
            }
            None => self.dropped += 1,
        }
        self.drain_events();
    }

    fn tick(&mut self, now: u64, out: &mut Vec<Send>) {
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
        // OD-150: the engine the host's own (loopback) game synced in last; free roam is the only
        // engine so far, so it is "freeroam" once the host's game is in, null before
        let host_in = self.is_player
            && self
                .engines
                .get(&EngineKind::FreeRoam)
                .is_some_and(|e| e.loopback_synced_at().is_some());
        if host_in {
            self.host_seen.set(true);
        }
        m.insert(
            "activity".into(),
            if self.host_seen.get() {
                json!("freeroam")
            } else {
                Value::Null
            },
        );
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
