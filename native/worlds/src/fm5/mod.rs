// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The Forza Motorsport 5 game module (`game: "fm5"`; OD-206, CR-S99 §5, W6): FM5's NerdCore
//! (`nerd`, its own widths) and the race server (`race`), ported from the LAN lab's
//! `tools/fm5-probe/{nerd,world}.py`; their golden vectors are `tools/fm5-probe/test_world.py`'s
//! (`tests`). Not run with the game from here: the lab's Python server is what raced.
//!
//! A world runs one private race lobby: `activity: "race"` (required in the public profile,
//! refused in the private one, as FH2's), at most 16 players (CR-S99 §1). It takes no other
//! game-scoped field and answers no game command. It may become a private official world (CR-S99:
//! private race lobbies); FM5 has no known session privacy message, so that marks the world, names
//! its leader in `world.state` and gives that leader the game's options (W16: the GameOptions
//! object's ownership, lobby spec §13), so the lobby's creator picks track and laps on FM5's own
//! options screen. A public world keeps them the server's.

pub mod nerd;
pub mod options;
pub mod race;

#[cfg(test)]
mod tests;

use std::net::SocketAddr;

use serde_json::{Map, Value, json};

use crate::{
    control::proto::{CmdError, CmdResult},
    core::{
        Profile,
        fields::Obj,
        game::{Game, GameCtx, GameEvent, GameWorld, Notice, OpenInfo, Send},
        rng::Rng,
    },
};
use race::Race;

pub const GAME_ID: &str = "fm5";
/// FM5's one activity (CR-S99 §1).
pub const ACTIVITY: &str = "race";
/// Seats per world (CR-S99 §1; the session's `maxMembersCount` 16).
pub const MAX_PLAYERS: u8 = 16;

/// The FM5 module (no process-wide state, no process command).
#[derive(Debug, Default)]
pub struct Fm5;

impl Game for Fm5 {
    fn id(&self) -> &'static str {
        GAME_ID
    }

    fn max_players(&self) -> u8 {
        MAX_PLAYERS
    }

    fn open(&self, info: OpenInfo) -> Result<(Box<dyn GameWorld>, Vec<Notice>), CmdError> {
        let o = info.request;
        if info.max_players > MAX_PLAYERS {
            return Err(o.bad("maxPlayers"));
        }
        let activity = match (info.profile, o.has("activity")) {
            (Profile::Private, true) => return Err(o.bad("activity")),
            (Profile::Private, false) => None,
            (Profile::Public, _) => match o.str("activity")? {
                ACTIVITY => Some(ACTIVITY),
                _ => return Err(o.bad("activity")),
            },
        };
        let world = Fm5World {
            race: Race::new(
                info.admit.clone(),
                Rng::new(info.rng.next_u64()),
                usize::from(info.max_players),
            ),
            activity,
            is_player: info.is_player,
            host_xuid: info.host_xuid,
            private: false,
        };
        Ok((Box::new(world), Vec::new()))
    }
}

/// One FM5 world's game side: its race lobby.
pub struct Fm5World {
    race: Race,
    /// A public world's activity (`None`: private).
    activity: Option<&'static str>,
    is_player: bool,
    host_xuid: u64,
    /// A private official world (one way; CR-S99, WORLDS_OFFICIAL.md §12.8b).
    private: bool,
}

impl Fm5World {
    fn flush(&mut self, out: &mut Vec<Send>) {
        out.append(&mut self.race.out);
    }
}

impl GameWorld for Fm5World {
    fn spec(&self) -> Value {
        json!({"activity": self.activity})
    }

    fn lobbies(&self) -> Vec<&'static str> {
        Vec::new()
    }

    fn command(&mut self, _op: &str, _request: Obj, _ctx: &mut GameCtx) -> Option<CmdResult> {
        None
    }

    fn receive(&mut self, data: &[u8], from: SocketAddr, now: u64, out: &mut Vec<Send>) {
        let from = crate::stun::plain_endpoint(from);
        // the hook's FH2 side channels have no FM5 counterpart
        if crate::stun::is_framed(data) {
            self.race.dropped += 1;
            return;
        }
        self.race.receive(data, from, now);
        self.flush(out);
    }

    fn tick(&mut self, now: u64, out: &mut Vec<Send>) {
        self.race.tick(now);
        self.flush(out);
    }

    fn next_tick(&self, _profile: Profile, now: u64) -> Option<u64> {
        // pings every 500 ms, resends after 400 ms, the grid every second
        (!self.race.is_idle()).then_some(now + 20)
    }

    fn synced(&self) -> usize {
        self.race.synced()
    }

    fn handshaking(&self) -> usize {
        self.race.handshaking()
    }

    fn kick(&mut self, xuid: u64, now: u64, out: &mut Vec<Send>) -> usize {
        let n = self.race.kick(xuid, now);
        self.flush(out);
        n
    }

    fn close_all(&mut self, out: &mut Vec<Send>) {
        self.race.close_all();
        self.flush(out);
    }

    fn state(&self) -> Map<String, Value> {
        let synced = self.race.synced();
        let xuids = self.race.xuids();
        let peers: Vec<String> = xuids.iter().map(u64::to_string).collect();
        let mut m = Map::new();
        m.insert("peers".into(), json!(peers));
        m.insert("lobbyPlayers".into(), json!(synced));
        m.insert("activity".into(), json!(ACTIVITY));
        m.insert("engines".into(), json!({ race::ENGINE: synced }));
        // OD-194: a launched race takes no newcomers (the lab never tried a join mid-race)
        if self.race.launched {
            m.insert("inProgress".into(), Value::Bool(true));
        }
        let host_in = self.is_player && xuids.contains(&self.host_xuid);
        m.insert("hostInWorld".into(), Value::Bool(host_in));
        if self.private {
            m.insert("private".into(), Value::Bool(true));
            if let Some(x) = self.race.leader() {
                m.insert("leader".into(), json!(x.to_string()));
            }
        }
        m
    }

    fn dropped(&self) -> u64 {
        self.race.dropped
    }

    fn occupants(&self, except: u64) -> usize {
        self.race.occupants(except)
    }

    fn take_events(&mut self) -> Vec<GameEvent> {
        std::mem::take(&mut self.race.events)
    }

    /// CR-S99: FM5's private race lobbies.
    fn supports_private(&self) -> bool {
        self.activity == Some(ACTIVITY)
    }

    /// FM5 has no known session privacy message (lobby spec: none found, unlike FH2's
    /// SessionHostSynced `PrivacyMode`): the world is marked private, its leader (the
    /// longest-present player, the engine's own rule, reported to the agent in `world.state`) is
    /// named for the invites route, and that leader is granted the GameOptions (W16).
    fn set_private(&mut self, out: &mut Vec<Send>) {
        if self.supports_private() {
            self.private = true;
            self.race.give_options_to_leader();
            self.flush(out);
        }
    }
}
