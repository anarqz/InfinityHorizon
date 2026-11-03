// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The Forza Horizon 2 game module (`game: "fh2"`, OD-186): the NerdCore codec (`codec`,
//! written from NERDCORE_WIRE.md, OD-187), the side channels (`carstate`; the retired livery channel is dropped), the engines (free roam,
//! car meets 3 and 6, Road Trip), the FH2 settings (`RelaySettings`), voice, the anti-cheat
//! watch and the game-data catalog (`gamedbRC.slt`).
//!
//! World-scoped FH2 fields of `world.open`: `races`, `voice`, `watch`, `settings`.
//!
//! Step 2: the free-roam engine is ported (contract §7); it runs behind a deny-all policy until
//! the admission snapshot is wired to it, so a world admits nobody yet (README, PORT_STATUS).

pub mod carstate;
pub mod codec;
pub mod engine;
pub mod settings;

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf};

use serde_json::{Map, Value, json};

use crate::{
    control::proto::{self, CmdError, CmdResult},
    core::{
        Profile,
        fields::{Obj, parse_xuid},
        game::{Game, GameCtx, GameWorld, Notice, OpenInfo, Send},
    },
};
use engine::{DenyAll, Engine, EngineKind, EngineSettings, Event, random::EngineRandom};
pub use settings::RelaySettings;

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

    pub fn as_str(self) -> &'static str {
        match self {
            VoiceMode::Off => "off",
            VoiceMode::Open => "open",
            VoiceMode::Proximity => "proximity",
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
        };
        let rng = &mut *info.rng;
        let random = EngineRandom::from_source(|| rng.next_u64());
        let free_roam = Engine::new(
            EngineKind::FreeRoam,
            engine_settings,
            random,
            Box::new(DenyAll),
        )
        .ok_or_else(|| CmdError::bad("settings"))?;
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
            roster_blocks: BTreeMap::new(),
            dropped: 0,
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
    /// `world.voiceBlocks` (the roster source of §6.4; the merge with the grant source comes
    /// with the voice port).
    roster_blocks: BTreeMap<u64, u64>,
    dropped: u64,
}

impl Fh2World {
    /// The engines' world-log lines go to the process log (indexes and ids only, never an
    /// identity or an address); the other events feed `world.state`, which is read fresh.
    fn drain_events(&mut self) {
        for e in self.engines.values_mut() {
            for ev in e.take_events() {
                if let Event::Note { id, args } = ev
                    && crate::log::enabled(crate::log::Level::Debug)
                {
                    crate::log::debug("world.note")
                        .field("note", id)
                        .field("args", Value::Array(args))
                        .emit();
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
            next.insert(x, until);
        }
        let changed = next != self.roster_blocks;
        self.roster_blocks = next;
        let mut m = Map::new();
        m.insert("changed".into(), Value::Bool(changed));
        Ok(m)
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
        m.insert("activity".into(), Value::Null);
        m.insert("engines".into(), Value::Object(engines));
        m.insert("hostInWorld".into(), Value::Bool(false));
        m
    }

    fn dropped(&self) -> u64 {
        self.dropped + self.engines.values().map(|e| e.dropped).sum::<u64>()
    }
}
