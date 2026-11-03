// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Team events (NR-10). An event is a team event when its options say NumTeams != 0 (the game's
//! own test), or in a Road Trip when it is the team twin of an online event (its id + 10000);
//! King and Infected never are. At the participants' selection the server shuffles them
//! (Fisher-Yates from the end), alternates red (0) and blue (1), writes each one's TeamId (player
//! object var 90, 1 bit) as a server update before the starting grid, and the grid keeps the
//! alternation. An event without teams sets every TeamId written before back to 0. Voice follows:
//! each participant hears only its hearing group while the event runs.

use std::sync::Arc;

use serde_json::json;

use super::{
    Engine,
    bits::Bits,
    key::ObjKey,
    message::{Message, ObjectChange},
    options::Fields,
    random::{LegacyRandom, SeedPurpose},
};
use crate::fh2::codec::object::{seq_forward, seq_next};

/// PlayerChoices TeamId on the player object, 1 bit: 0 red, 1 blue.
pub const TEAM_VARIABLE: u8 = 90;
pub const TEAM_RED: u8 = 0;
pub const TEAM_BLUE: u8 = 1;
/// Every online event exists twice: its id and, as its team variant, id + 10000.
pub const TEAM_VARIANT_OFFSET: i32 = 10_000;

impl Engine {
    /// The running team event's teams by XUID (0 red, 1 blue), in engine order; empty outside one.
    pub fn teams(&self) -> Vec<(u64, u8)> {
        self.peers
            .iter()
            .filter_map(|p| p.team.map(|t| (p.xuid, t)))
            .collect()
    }

    /// A team event runs (from its participants' selection to its end).
    pub fn team_event_active(&self) -> bool {
        self.team_event
    }

    fn is_team_event(&self) -> bool {
        let fields = Fields::read(self.ev.options.records());
        if fields.as_ref().is_some_and(|f| f.tag().is_some()) {
            return false;
        }
        if self
            .road_trip_current_race()
            .is_some_and(|r| r >= TEAM_VARIANT_OFFSET)
        {
            return true;
        }
        fields.and_then(|f| f.num_teams()).is_some_and(|n| n > 0)
    }

    /// Participants were selected: teams for a team event, TeamId back to 0 otherwise.
    pub(super) fn assign_teams(&mut self) {
        self.team_pending.clear();
        for p in &mut self.peers {
            p.team = None;
        }
        if !self.is_team_event() {
            let was = self.team_event;
            self.team_event = false;
            let reset: Vec<u64> = self
                .peers
                .iter()
                .filter(|p| p.synced && p.team_written.is_some_and(|w| w != TEAM_RED))
                .map(|p| p.serial)
                .collect();
            for s in reset {
                self.write_team(s, TEAM_RED);
            }
            if was {
                self.voice_groups_changed();
            }
            return;
        }
        let Some(mut order) = self.ev.race.as_ref().map(|r| r.participants.clone()) else {
            return;
        };
        if self.team_random.is_none() {
            let seed = self
                .settings
                .relay
                .team_seed
                .unwrap_or_else(|| self.random.seed(SeedPurpose::Team));
            self.team_random = Some(LegacyRandom::new(seed));
        }
        let random = self.team_random.as_mut().expect("set above");
        for i in (1..order.len()).rev() {
            let j = random.next_below(i as i32 + 1) as usize;
            order.swap(i, j);
        }
        self.team_event = true;
        for (i, index) in order.iter().enumerate() {
            let Some(serial) = self
                .peers
                .iter()
                .find(|p| p.index == *index)
                .map(|p| p.serial)
            else {
                continue;
            };
            let team = if i % 2 == 0 { TEAM_RED } else { TEAM_BLUE };
            if let Some(p) = self.peer_mut(serial) {
                p.team = Some(team);
            }
            if !self.write_team(serial, team) {
                self.team_pending.insert(*index);
            }
        }
        if let Some(race) = self.ev.race.as_mut() {
            race.grid = Some(order.clone());
        }
        let pick = |odd: usize| -> Vec<u8> {
            order
                .iter()
                .enumerate()
                .filter(|(i, _)| i % 2 == odd)
                .map(|(_, x)| *x)
                .collect()
        };
        let (red, blue) = (pick(0), pick(1));
        self.note("teams.drawn", vec![json!(red), json!(blue)]);
        self.voice_groups_changed();
    }

    /// The server's TeamId for a player: to its owner, and to everybody else on its relayed
    /// object. False: no player object yet (written when it is created).
    fn write_team(&mut self, serial: u64, team: u8) -> bool {
        let Some(p) = self.peer_mut(serial) else {
            return false;
        };
        if !p.synced {
            return false;
        }
        let Some(id) = p.objects.iter().find(|(k, _)| k.0 == 1).map(|(k, _)| k.1) else {
            return false;
        };
        let index = p.index;
        let mut payload = Bits::new();
        payload
            .push(u64::from(TEAM_VARIABLE), 8)
            .push(u64::from(team), 1);
        let seq = p.player_sequence.map_or(0, seq_next);
        p.player_sequence = Some(seq);
        let own = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 1,
            key: ObjKey::server(id),
            ty: 0,
            sequence: seq,
            vars: payload.clone(),
        })));
        self.send_sequenced(serial, vec![own]);
        let Some(obj) = self
            .peer_mut(serial)
            .and_then(|p| p.objects.get_mut(&(1, id)))
        else {
            return false;
        };
        obj.injected = seq_next(obj.injected);
        obj.team = Some(team);
        let seq = seq_forward(obj.last, obj.injected);
        let m = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 1,
            key: ObjKey::relay(id, index),
            ty: 0,
            sequence: seq,
            vars: payload,
        })));
        self.forward(serial, vec![m], true);
        if let Some(p) = self.peer_mut(serial) {
            p.team_written = Some(team);
        }
        true
    }

    /// A player's router-1 object was created (it starts at TeamId 0): the running event's team
    /// reaches it now.
    pub(super) fn team_object_created(&mut self, serial: u64) {
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        p.team_written = None;
        let (Some(team), index) = (p.team, p.index) else {
            return;
        };
        if !self.write_team(serial, team) {
            return;
        }
        if self.team_pending.remove(&index) {
            self.voice_groups_changed();
        }
    }

    /// A participant left the event: it is outside the event for voice; its TeamId stays.
    pub(super) fn team_player_left(&mut self, index: u8) {
        if !self.team_event {
            return;
        }
        let Some(p) = self
            .peers
            .iter_mut()
            .find(|p| p.index == index && p.team.is_some())
        else {
            return;
        };
        p.team = None;
        self.team_pending.remove(&index);
        self.voice_groups_changed();
    }

    /// The event ended: the lobby's voice applies again; TeamIds stay until the next event.
    pub(super) fn team_event_ended(&mut self) {
        if !self.team_event {
            return;
        }
        self.team_event = false;
        self.team_pending.clear();
        for p in &mut self.peers {
            p.team = None;
        }
        self.note("teams.ended", Vec::new());
        self.voice_groups_changed();
    }

    /// §5c rule 2a: a player's hearing group. Outside team events everybody shares one (0); in a
    /// team event a participant's is its team (1 red, 2 blue), the others' 0; a participant whose
    /// team the game does not have yet shares one with nobody (negative, unique).
    pub(super) fn hearing_group(&self, serial: u64) -> i32 {
        let Some(p) = self.peer(serial) else { return 0 };
        if !self.team_event
            || !self
                .ev
                .race
                .as_ref()
                .is_some_and(|r| r.participants.contains(&p.index))
        {
            return 0;
        }
        match p.team {
            Some(t) if !self.team_pending.contains(&p.index) => 1 + i32::from(t),
            _ => -1 - i32::from(p.index),
        }
    }

    /// A player's team in the running team event.
    pub(super) fn team_of(&self, serial: u64) -> Option<u8> {
        let p = self.peer(serial)?;
        if !self.team_event
            || !self
                .ev
                .race
                .as_ref()
                .is_some_and(|r| r.participants.contains(&p.index))
        {
            return None;
        }
        p.team
    }
}
