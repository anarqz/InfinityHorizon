// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Car meet game-mode invitations (CarMeetSynced, router 11; contract §4.4). A player starts
//! Online Free Roam from the meet (26); the others get a 20 s countdown and join or cancel (27).
//! When it ends the engine asks its world for one shared session (`world.gameSession.request`)
//! for the starter and everybody who joined; the joiners are told to wait (29, wait), then where
//! to go (32, plus CarMeetSynced vars 16/17). Any failure, the starter leaving or backing out of
//! its own invitation (27), or a Road Trip countdown that ends with nobody but its starter
//! (OD-183: no solo Road Trip), cancels it for everybody (invite type 0); the players already
//! waiting first get 29(none), which ends their wait. Updates go out reliably, never
//! re-broadcast on a timer.

use std::sync::Arc;

use serde_json::json;

use super::{
    Engine, Event,
    bits::Bits,
    key::ObjKey,
    message::{Message, ObjectChange},
};
use crate::fh2::codec::object::seq_next;

pub const INVITE_FREE_ROAM: u8 = 1;
/// The Road Trip invitation (`CarMeetInviteType` 2; served only where the world allows it,
/// OD-191: an official car meet lobby).
pub const INVITE_ROAD_TRIP: u8 = 2;
pub const CAR_MEET_JOIN_MS: i64 = 20_000;
pub const CAR_MEET_CREATE_MS: i64 = 15_000;
pub const CAR_MEET_READY_MS: i64 = 30_000;
/// How long the official car meet waits after telling the starter before it tells the joiners: 0
/// since every reader sees itself first (the server's reader-first answers); kept as a knob.
pub const CAR_MEET_JOINER_DELAY_MS: i64 = 0;
/// A session or template name's longest form (CarMeetSynced 16/17, 32).
pub const CAR_MEET_NAME_MAX: usize = 64;
const PENDING_NONE: u64 = 0;
const PENDING_WAIT: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum InviteStage {
    #[default]
    Idle,
    Joining,
    Creating,
    Ready,
}

impl InviteStage {
    fn name(self) -> &'static str {
        match self {
            InviteStage::Idle => "Idle",
            InviteStage::Joining => "Joining",
            InviteStage::Creating => "Creating",
            InviteStage::Ready => "Ready",
        }
    }
}

/// One car meet engine's invitation.
#[derive(Debug, Default)]
pub(super) struct CarMeet {
    stage: InviteStage,
    kind: u8,
    /// Serials (indexes are reused).
    starter: Option<u64>,
    joined: Vec<u64>,
    until: i64,
    mode32: u32,
    request: u32,
    session: Option<(String, String)>,
    pub sequence: u8,
    /// The official car meet lobby (OD-190/OD-191): Road Trip invitations are asked for too and
    /// the invitation's players are shown in JoinTypes; off: the reference (free roam only,
    /// JoinTypes never sent, as the D2 vectors pin).
    official: bool,
    /// The JoinTypes slots last sent as GameMode (bit k: JoinTypes[k], CarMeetSynced var 4+k).
    join_slots: u16,
    /// Official lobby: the joiners told of the session only once the starter had time to join it
    /// first (`late_at`), as in the game's own flow where the starter makes the session and is in
    /// it before the others hear of it (live test 2026-11-03: joiners that read it before the
    /// starter was in left it).
    late: Vec<u64>,
    late_at: i64,
    /// The session-vars update the late players get (the starter got it already).
    late_update: Option<Arc<Bits>>,
}

/// JoinTypes (CarMeetSynced vars 4-15, `CarMeetJoinType`: 0 None, 1 WagerRace, 2 GameMode).
const JOIN_TYPE_FIRST_VAR: u64 = 4;
const JOIN_TYPES: usize = 12;
const JOIN_TYPE_GAME_MODE: u64 = 2;

/// CarMeetSynced variables: 1 InviteCountDownTime and, when given, 16/17 the session's name and
/// template (a 7-bit count of at most 64, then UTF-16 units).
fn car_meet_vars(session: Option<&(String, String)>, countdown_ms: i64) -> Bits {
    let mut out = Bits::new();
    out.push(1, 5).push(countdown_ms as u64, 32);
    if let Some((name, template)) = session {
        for (variable, text) in [(16u64, name), (17, template)] {
            let units: Vec<u16> = text.encode_utf16().take(CAR_MEET_NAME_MAX).collect();
            out.push(variable, 5).push(units.len() as u64, 7);
            for u in units {
                out.push(u64::from(u), 16);
            }
        }
    }
    out
}

fn notify_pending(action: u64) -> Arc<Bits> {
    let mut body = Bits::new();
    body.push(action, 2);
    Arc::new(Message::encode(&Message::Opaque { id: 29, body }))
}

fn session_ready(name: &str, template: &str) -> Arc<Bits> {
    let mut body = Bits::new();
    body.byte_string(name.as_bytes())
        .byte_string(template.as_bytes());
    Arc::new(Message::encode(&Message::Opaque { id: 32, body }))
}

fn printable(text: &str) -> bool {
    !text.is_empty()
        && text.chars().count() <= CAR_MEET_NAME_MAX
        && text.chars().all(|c| (' '..='~').contains(&c))
}

impl Engine {
    /// CarMeetSynced as it is now (late joiners get exactly this in their snapshot).
    pub(super) fn car_meet_state(&self) -> Bits {
        let m = &self.car_meet;
        if m.stage == InviteStage::Idle {
            return car_meet_vars(None, 10_000);
        }
        let mut out = Bits::new();
        out.push(0, 5).push(u64::from(m.kind), 2);
        out.append(&car_meet_vars(m.session.as_ref(), CAR_MEET_JOIN_MS));
        out.push(3, 5).push(u64::from(m.mode32), 32);
        self.join_types_state(&mut out);
        out
    }

    /// The car meet's server-owned object in a joiner's snapshot.
    pub(super) fn append_car_meet_snapshot(&self, snapshot: &mut Vec<Bits>) {
        if self.settings.car_meet_lobby.is_none() {
            return;
        }
        snapshot.push(Message::encode(&Message::Object(ObjectChange {
            is_create: true,
            router_id: 11,
            key: ObjKey::own(0),
            ty: 0,
            sequence: self.car_meet.sequence,
            vars: self.car_meet_state(),
        })));
    }

    fn car_meet_update(&mut self, variables: Bits) {
        let update = self.car_meet_message(variables);
        for s in self.synced_serials_except(None) {
            self.send_sequenced(s, vec![update.clone()]);
        }
    }

    /// The next CarMeetSynced update, encoded once (every player gets the same sequence).
    fn car_meet_message(&mut self, variables: Bits) -> Arc<Bits> {
        self.car_meet.sequence = seq_next(self.car_meet.sequence);
        Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 11,
            key: ObjKey::own(0),
            ty: 0,
            sequence: self.car_meet.sequence,
            vars: variables,
        })))
    }

    fn invite_cleared() -> Bits {
        let mut b = Bits::new();
        b.push(0, 5).push(0, 2);
        b
    }

    /// The end of an invitation: var 3 JoinEndTimeGameMode back to 0 in every car meet (OD-195,
    /// SAURON's F-4, carmeet-invite.md: the game hides (Y) from everybody while var 3 is non-zero
    /// and past, so the old window end left nobody able to start an invitation again; a late
    /// joiner's snapshot, var 3 absent, was the only way back). The official car meet lobby also
    /// empties the session name and template (vars 16/17): a joiner's game takes the session from
    /// them, so a name left from an earlier round sent it to that old, empty session (local test
    /// 2026-11-03); the player-hosted car meet keeps the reference behaviour there.
    fn invite_end_update(&self) -> Bits {
        let mut b = Self::invite_cleared();
        b.push(3, 5).push(0, 32);
        if self.car_meet.official {
            b.push(16, 5).push(0, 7).push(17, 5).push(0, 7);
        }
        b
    }

    /// The official car meet lobby (OD-190/OD-191): Road Trip sessions are asked for too (the
    /// agent opens a group world for them; Showdown and other types stay ignored), and the
    /// invitation's players are shown in JoinTypes, so the games count the joiners.
    pub fn set_official_car_meet(&mut self, on: bool) {
        self.car_meet.official = on;
    }

    /// The JoinTypes slots that should read GameMode now: the invitation's players (the starter
    /// and every joiner still here), each at its rank among the engine's synced players by index
    /// (the game's player order; FH2MP's slot is the same rank, unproven), the first 12.
    fn join_slots_now(&self) -> u16 {
        // OD-195: every car meet counts its invitation's players (it was the official lobby's only;
        // a private lobby's car meet showed 0/12 with both players in, live 2026-11-04)
        if self.car_meet.stage == InviteStage::Idle {
            return 0;
        }
        let mut order: Vec<u8> = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| p.index)
            .collect();
        order.sort_unstable();
        let mut mask = 0u16;
        for s in &self.car_meet.joined {
            let Some(index) = self.peer(*s).filter(|p| p.synced).map(|p| p.index) else {
                continue;
            };
            if let Some(rank) = order.iter().position(|i| *i == index)
                && rank < JOIN_TYPES
            {
                mask |= 1 << rank;
            }
        }
        mask
    }

    /// The official lobby's JoinTypes, sent reliably to every synced player when they change (a
    /// join, a cancel, a player leaving or ranking differently, the invitation's end).
    fn update_join_types(&mut self) {
        let now = self.join_slots_now();
        let old = self.car_meet.join_slots;
        if now == old {
            return;
        }
        let mut vars = Bits::new();
        for k in 0..JOIN_TYPES {
            if (now ^ old) >> k & 1 == 1 {
                let value = if now >> k & 1 == 1 {
                    JOIN_TYPE_GAME_MODE
                } else {
                    0
                };
                vars.push(JOIN_TYPE_FIRST_VAR + k as u64, 5).push(value, 2);
            }
        }
        self.car_meet.join_slots = now;
        self.car_meet_update(vars);
        self.note("carmeet.join_types", vec![json!(now.count_ones())]);
    }

    /// The JoinTypes last sent, for a late joiner's snapshot.
    fn join_types_state(&self, out: &mut Bits) {
        for k in 0..JOIN_TYPES {
            if self.car_meet.join_slots >> k & 1 == 1 {
                out.push(JOIN_TYPE_FIRST_VAR + k as u64, 5)
                    .push(JOIN_TYPE_GAME_MODE, 2);
            }
        }
    }

    /// 26: a player starts an invitation.
    pub(super) fn invite_request(&mut self, serial: u64, body: &Bits) {
        if self.settings.car_meet_lobby.is_none() {
            return;
        }
        let Some(index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        let Ok(kind) = body.reader().read(2) else {
            return;
        };
        let kind = kind as u8;
        let served =
            kind == INVITE_FREE_ROAM || (kind == INVITE_ROAD_TRIP && self.car_meet.official);
        if !served {
            // Road Trip and Showdown invitations: their sessions do not exist on the platform yet
            self.peer_noted(
                serial,
                "carmeet.invite_ignored_type",
                vec![json!(kind), json!(index)],
            );
            return;
        }
        if self.car_meet.stage != InviteStage::Idle {
            let stage = self.car_meet.stage.name();
            self.peer_noted(
                serial,
                "carmeet.invite_ignored_busy",
                vec![json!(index), json!(stage)],
            );
            return;
        }
        let mode32 = self.now_wire().wrapping_add(CAR_MEET_JOIN_MS as u32);
        let m = &mut self.car_meet;
        m.stage = InviteStage::Joining;
        m.kind = kind;
        m.starter = Some(serial);
        m.joined = vec![serial];
        m.until = self.now + CAR_MEET_JOIN_MS;
        m.mode32 = mode32;
        m.session = None;
        m.late.clear();
        m.late_update = None;
        let mut vars = Bits::new();
        vars.push(0, 5)
            .push(u64::from(kind), 2)
            .push(1, 5)
            .push(CAR_MEET_JOIN_MS as u64, 32)
            .push(3, 5)
            .push(u64::from(mode32), 32);
        self.car_meet_update(vars);
        self.note(
            "carmeet.invite",
            vec![json!(index), json!(CAR_MEET_JOIN_MS)],
        );
        self.update_join_types();
    }

    /// 27: a player joins or leaves the running invitation.
    pub(super) fn join_or_cancel(&mut self, serial: u64, body: &Bits) {
        if self.car_meet.stage != InviteStage::Joining {
            return;
        }
        let Some(index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        let mut r = body.reader();
        let (Ok(bit), Ok(ty)) = (r.read(1), r.read(2)) else {
            return;
        };
        if ty == 1 {
            return; // a wager-race answer
        }
        // a game-mode invitation's bit reads the other way round: accepting arrives as 0
        let join = bit == 0;
        if self.car_meet.starter == Some(serial) {
            // live test 2026-11-04: the starter backing out of its own invitation was ignored, so
            // the countdown ran on and made a session its game no longer waited for (it froze)
            if !join {
                self.peer_noted(serial, "carmeet.starter_left", vec![json!(index)]);
                self.cancel_invite("its starter backed out");
            }
            return;
        }
        let m = &mut self.car_meet;
        if join && !m.joined.contains(&serial) {
            m.joined.push(serial);
        } else if !join {
            m.joined.retain(|s| *s != serial);
        }
        let n = m.joined.len();
        let id = if join {
            "carmeet.joined"
        } else {
            "carmeet.left"
        };
        self.peer_noted(serial, id, vec![json!(index), json!(n)]);
        self.update_join_types();
    }

    pub(super) fn tick_car_meet(&mut self) {
        let now = self.now;
        if !self.car_meet.late.is_empty()
            && now >= self.car_meet.late_at
            && let Some(session) = self.car_meet.session.clone()
        {
            let late = std::mem::take(&mut self.car_meet.late);
            let starter = self.car_meet.starter;
            if let Some(update) = self.car_meet.late_update.take() {
                for s in self.synced_serials_except(starter) {
                    self.send_sequenced(s, vec![update.clone()]);
                }
            }
            let ready = session_ready(&session.0, &session.1);
            let alive: Vec<u64> = late.into_iter().filter(|s| self.alive(*s)).collect();
            for s in &alive {
                self.send_sequenced(*s, vec![ready.clone()]);
            }
            self.note("carmeet.session_ready_joiners", vec![json!(alive.len())]);
        }
        match self.car_meet.stage {
            InviteStage::Joining if self.car_meet.starter.is_none_or(|s| !self.alive(s)) => {
                self.cancel_invite("its starter left");
            }
            InviteStage::Joining if now >= self.car_meet.until => {
                let joined: Vec<u64> = self
                    .car_meet
                    .joined
                    .iter()
                    .copied()
                    .filter(|s| self.alive(*s))
                    .collect();
                self.car_meet.joined = joined.clone();
                // OD-183: a Road Trip is for two players or more; nobody joined the starter
                if self.car_meet.kind == INVITE_ROAD_TRIP && joined.len() < 2 {
                    self.cancel_invite("nobody joined the road trip");
                    return;
                }
                if !self.policy.can_create_game_session() {
                    self.cancel_invite("this host cannot create sessions");
                    return;
                }
                let m = &mut self.car_meet;
                m.stage = InviteStage::Creating;
                m.until = now + CAR_MEET_CREATE_MS;
                m.request = m.request.wrapping_add(1);
                let (request, kind) = (m.request, m.kind);
                // nobody creates the session on the game side: everybody waits for the platform's
                let wait = notify_pending(PENDING_WAIT);
                for s in &joined {
                    self.send_sequenced(*s, vec![wait.clone()]);
                }
                self.note("carmeet.countdown_over", vec![json!(joined.len())]);
                let xuids = joined
                    .iter()
                    .filter_map(|s| self.peer(*s).map(|p| p.xuid))
                    .collect();
                self.events.push(Event::CreateGameSession {
                    id: request,
                    kind,
                    xuids,
                });
            }
            InviteStage::Creating if now >= self.car_meet.until => {
                self.cancel_invite("the session did not arrive in time");
            }
            InviteStage::Ready if now >= self.car_meet.until => {
                let m = &mut self.car_meet;
                m.stage = InviteStage::Idle;
                m.starter = None;
                m.joined.clear();
                m.session = None;
                self.car_meet_update(self.invite_end_update());
            }
            _ => {}
        }
        // a player who left, or a rank that moved, shows at once
        self.update_join_types();
    }

    /// The platform's answer for request `id` (`world.gameSession.answer`): the shared session,
    /// or `None` when it was refused. False: stale (not the invitation being created).
    pub fn game_session_created(
        &mut self,
        id: u32,
        name: Option<&str>,
        template: Option<&str>,
        now_ms: i64,
    ) -> bool {
        self.now = self.now.max(now_ms);
        if self.car_meet.stage != InviteStage::Creating || id != self.car_meet.request {
            return false;
        }
        let (Some(name), Some(template)) = (name, template) else {
            self.cancel_invite("the platform refused the session");
            return true;
        };
        if !printable(name) || !printable(template) {
            self.cancel_invite("the platform refused the session");
            return true;
        }
        let session = (name.to_string(), template.to_string());
        let m = &mut self.car_meet;
        m.stage = InviteStage::Ready;
        m.until = self.now + CAR_MEET_READY_MS;
        m.session = Some(session.clone());
        let ready = session_ready(name, template);
        let joined: Vec<u64> = self
            .car_meet
            .joined
            .iter()
            .copied()
            .filter(|s| self.alive(*s))
            .collect();
        if self.car_meet.official {
            // the starter first (the session vars, then 32); every other synced player gets the
            // same vars update, and the joiners 32, once the starter had time to join
            let starter = self.car_meet.starter;
            let update = self.car_meet_message(car_meet_vars(Some(&session), CAR_MEET_JOIN_MS));
            for s in joined.iter().filter(|s| Some(**s) == starter) {
                self.send_sequenced(*s, vec![update.clone(), ready.clone()]);
            }
            self.car_meet.late_update = Some(update);
            self.car_meet.late = joined
                .iter()
                .copied()
                .filter(|s| Some(*s) != starter)
                .collect();
            self.car_meet.late_at = self.now + CAR_MEET_JOINER_DELAY_MS;
        } else {
            self.car_meet_update(car_meet_vars(Some(&session), CAR_MEET_JOIN_MS));
            for s in &joined {
                self.send_sequenced(*s, vec![ready.clone()]);
            }
        }
        self.note("carmeet.ready", vec![json!(joined.len())]);
        if self.car_meet.official {
            // the next live test reads this (counts only)
            self.note("carmeet.session_ready", vec![json!(joined.len())]);
        }
        true
    }

    fn cancel_invite(&mut self, why: &str) {
        // T33: the players told to wait sit on WAITING FOR SESSION until the pending action is
        // cleared; sent before the CarMeetSynced update, on the same reliable channel
        if self.car_meet.stage == InviteStage::Creating {
            let waiting: Vec<u64> = self
                .car_meet
                .joined
                .iter()
                .copied()
                .filter(|s| self.alive(*s))
                .collect();
            if !waiting.is_empty() {
                let release = notify_pending(PENDING_NONE);
                for s in &waiting {
                    self.send_sequenced(*s, vec![release.clone()]);
                }
                self.note("carmeet.released", vec![json!(waiting.len())]);
            }
        }
        let m = &mut self.car_meet;
        m.stage = InviteStage::Idle;
        m.starter = None;
        m.joined.clear();
        m.session = None;
        m.late.clear();
        m.late_update = None;
        self.car_meet_update(self.invite_end_update());
        self.note("carmeet.cancelled", vec![json!(why)]);
        self.update_join_types();
    }

    pub(super) fn car_meet_peer_left(&mut self, serial: u64) {
        self.car_meet.joined.retain(|s| *s != serial);
    }
}

#[cfg(test)]
mod tests {
    use super::{super::testkit, *};
    use crate::fh2::RelaySettings;

    /// Admits everyone and has a controller to ask for sessions.
    struct Asks;
    impl super::super::Policy for Asks {
        fn has_authorize(&self) -> bool {
            false
        }
        fn has_authorize_hook(&self) -> bool {
            false
        }
        fn authorize(&mut self, _: u64, _: std::net::SocketAddr, _: i64) -> bool {
            true
        }
        fn authorize_hook(&mut self, _: u64, _: std::net::SocketAddr, _: i64) -> bool {
            true
        }
        fn awaits_proof(&mut self, _: u64, _: i64) -> Option<bool> {
            None
        }
        fn can_create_game_session(&self) -> bool {
            true
        }
    }

    fn car_meet() -> Engine {
        let settings = super::super::EngineSettings {
            relay: RelaySettings::new(),
            max_players: 12,
            races: true,
            car_meet_lobby: Some(3),
            enforce_car_rule: false,
            road_trip: None,
        };
        let seeds = super::super::random::Seeds {
            weather: 1,
            tag: 2,
            team: 3,
            attempt: 4,
            transaction: 5,
        };
        Engine::new(
            super::super::EngineKind::CarMeet3,
            settings,
            super::super::random::EngineRandom::new(seeds),
            Box::new(Asks),
        )
        .expect("valid settings")
    }

    /// The kinds of the sessions asked for after one invitation of `kind` and its countdown.
    fn asked(road_trip: bool, kind: u8) -> Vec<u8> {
        let mut e = car_meet();
        e.set_official_car_meet(road_trip);
        let starter = testkit::seat(&mut e, 1, 1_000_000_000_000_102);
        let joiner = testkit::seat(&mut e, 2, 1_000_000_000_000_104);
        let mut body = Bits::new();
        body.push(u64::from(kind), 2).push(0, 32);
        e.invite_request(starter, &body);
        join(&mut e, joiner, true);
        e.now += CAR_MEET_JOIN_MS + 1;
        e.tick_car_meet();
        e.take_events()
            .into_iter()
            .filter_map(|ev| match ev {
                Event::CreateGameSession { kind, xuids, .. } => {
                    assert_eq!(xuids, vec![1_000_000_000_000_102, 1_000_000_000_000_104]);
                    Some(kind)
                }
                _ => None,
            })
            .collect()
    }

    /// Every reliable message waiting for or in flight to `serial`.
    fn reliable(e: &Engine, serial: u64) -> Vec<Bits> {
        let p = e.peer(serial).expect("seated");
        p.in_flight
            .iter()
            .flat_map(|r| r.messages.iter())
            .chain(p.queue.iter())
            .map(|m| (**m).clone())
            .collect()
    }

    fn notes(e: &mut Engine, id: &str) -> Vec<Vec<serde_json::Value>> {
        e.take_events()
            .into_iter()
            .filter_map(|ev| match ev {
                Event::Note { id: n, args } if n == id => Some(args),
                _ => None,
            })
            .collect()
    }

    fn join(e: &mut Engine, serial: u64, accept: bool) {
        let mut body = Bits::new();
        // a game-mode answer: accepting arrives as 0
        body.push(u64::from(!accept), 1).push(2, 2);
        e.join_or_cancel(serial, &body);
    }

    fn road_trip_invite(e: &mut Engine, starter: u64) {
        let mut body = Bits::new();
        body.push(u64::from(INVITE_ROAD_TRIP), 2).push(0, 32);
        e.invite_request(starter, &body);
    }

    fn sessions_asked(e: &mut Engine) -> usize {
        e.take_events()
            .into_iter()
            .filter(|ev| matches!(ev, Event::CreateGameSession { .. }))
            .count()
    }

    /// Live test 2026-11-04: the starter backs out after the only joiner left; the countdown
    /// must not run on into a session for a game that no longer waits for one.
    #[test]
    fn the_starter_backing_out_cancels_its_invitation() {
        let mut e = car_meet();
        e.set_official_car_meet(true);
        let starter = testkit::seat(&mut e, 1, 1_000_000_000_000_102);
        let joiner = testkit::seat(&mut e, 2, 1_000_000_000_000_104);
        road_trip_invite(&mut e, starter);
        join(&mut e, joiner, true);
        join(&mut e, joiner, false);
        join(&mut e, starter, true); // the starter's own accept changes nothing
        assert_eq!(e.car_meet.stage, InviteStage::Joining);
        join(&mut e, starter, false);
        assert_eq!(e.car_meet.stage, InviteStage::Idle, "cancelled at once");
        assert_eq!(
            notes(&mut e, "carmeet.cancelled"),
            vec![vec![json!("its starter backed out")]]
        );
        e.now += CAR_MEET_JOIN_MS + 1;
        e.tick_car_meet();
        assert_eq!(sessions_asked(&mut e), 0);
    }

    /// OD-183: a Road Trip countdown that ends with its starter alone asks for no session.
    #[test]
    fn a_road_trip_nobody_joined_is_cancelled_when_its_countdown_ends() {
        let mut e = car_meet();
        e.set_official_car_meet(true);
        let starter = testkit::seat(&mut e, 1, 1_000_000_000_000_102);
        road_trip_invite(&mut e, starter);
        e.now += CAR_MEET_JOIN_MS + 1;
        e.tick_car_meet();
        assert_eq!(e.car_meet.stage, InviteStage::Idle);
        let events = e.take_events();
        assert!(
            !events
                .iter()
                .any(|ev| matches!(ev, Event::CreateGameSession { .. }))
        );
        assert!(events.iter().any(|ev| matches!(
            ev,
            Event::Note { id: "carmeet.cancelled", args } if args == &vec![json!("nobody joined the road trip")]
        )));
    }

    #[test]
    fn the_official_lobby_shows_its_joiners_and_sends_the_session_to_each() {
        let mut e = car_meet();
        e.set_official_car_meet(true);
        let starter = testkit::seat(&mut e, 3, 1_000_000_000_000_103);
        let joiner = testkit::seat(&mut e, 1, 1_000_000_000_000_101);
        let bystander = testkit::seat(&mut e, 7, 1_000_000_000_000_107);
        let mut body = Bits::new();
        body.push(u64::from(INVITE_FREE_ROAM), 2).push(0, 32);
        e.invite_request(starter, &body);
        // ranks by index: joiner (1) 0, starter (3) 1, bystander (7) 2
        assert_eq!(e.car_meet.join_slots, 0b010);
        assert_eq!(notes(&mut e, "carmeet.join_types"), vec![vec![json!(1)]]);
        let before = reliable(&e, bystander).len();
        join(&mut e, joiner, true);
        assert_eq!(e.car_meet.join_slots, 0b011, "the count goes up at once");
        assert_eq!(
            reliable(&e, bystander).len(),
            before + 1,
            "everybody sees it"
        );
        assert_eq!(notes(&mut e, "carmeet.join_types"), vec![vec![json!(2)]]);
        // a late snapshot carries the JoinTypes too
        let mut want = Bits::new();
        want.push(0, 5).push(u64::from(INVITE_FREE_ROAM), 2);
        want.append(&car_meet_vars(None, CAR_MEET_JOIN_MS));
        want.push(3, 5).push(u64::from(e.car_meet.mode32), 32);
        want.push(4, 5).push(2, 2).push(5, 5).push(2, 2);
        assert_eq!(e.car_meet_state(), want);
        join(&mut e, joiner, false);
        assert_eq!(e.car_meet.join_slots, 0b010, "a cancel takes it back");
        join(&mut e, joiner, true);
        assert_eq!(e.car_meet.join_slots, 0b011);
        e.take_events();
        // the countdown ends: one session for both, then 32 to each of them, not the bystander
        e.now += CAR_MEET_JOIN_MS + 1;
        e.tick_car_meet();
        let id = e
            .take_events()
            .into_iter()
            .find_map(|ev| match ev {
                Event::CreateGameSession { id, xuids, .. } => {
                    assert_eq!(xuids, vec![1_000_000_000_000_103, 1_000_000_000_000_101]);
                    Some(id)
                }
                _ => None,
            })
            .expect("asked");
        let (name, template) = ("fh2inf-invite-a-b", "defaulttemplate");
        assert!(e.game_session_created(id, Some(name), Some(template), e.now));
        let ready = (*session_ready(name, template)).clone();
        // the starter hears of the session first; the joiner once the starter had time to join it
        assert!(reliable(&e, starter).contains(&ready), "32 queued reliably");
        assert!(!reliable(&e, joiner).contains(&ready));
        assert_eq!(notes(&mut e, "carmeet.session_ready"), vec![vec![json!(2)]]);
        e.now += CAR_MEET_JOINER_DELAY_MS;
        e.tick_car_meet();
        assert!(reliable(&e, joiner).contains(&ready), "32 queued reliably");
        assert!(!reliable(&e, bystander).contains(&ready));
        assert_eq!(
            notes(&mut e, "carmeet.session_ready_joiners"),
            vec![vec![json!(1)]]
        );
        // the ready window ends: JoinTypes cleared for everybody
        e.now += CAR_MEET_READY_MS + 1;
        e.tick_car_meet();
        assert_eq!(e.car_meet.join_slots, 0);
        // ... var 3 back to 0 (OD-195: (Y) again for everybody), and the session name and template
        // are emptied, so a later round never inherits them
        let mut cleared = Engine::invite_cleared();
        cleared.push(3, 5).push(0, 32);
        cleared.push(16, 5).push(0, 7).push(17, 5).push(0, 7);
        assert_eq!(e.invite_end_update(), cleared);
    }

    #[test]
    fn a_player_hosted_car_meet_counts_its_joiners_too() {
        let mut e = car_meet();
        let starter = testkit::seat(&mut e, 1, 1_000_000_000_000_101);
        let joiner = testkit::seat(&mut e, 2, 1_000_000_000_000_102);
        let mut body = Bits::new();
        body.push(u64::from(INVITE_FREE_ROAM), 2).push(0, 32);
        e.invite_request(starter, &body);
        // the end of an invitation: the type cleared and var 3 back to 0 (OD-195), no 16/17
        let mut cleared = Engine::invite_cleared();
        cleared.push(3, 5).push(0, 32);
        assert_eq!(e.invite_end_update(), cleared);
        // OD-195: the starter and every joiner are counted (JoinTypes), as in the official lobby
        assert_eq!(e.car_meet.join_slots, 0b01);
        let before = reliable(&e, joiner).len();
        join(&mut e, joiner, true);
        assert_eq!(e.car_meet.join_slots, 0b11);
        assert_eq!(
            reliable(&e, joiner).len(),
            before + 1,
            "everybody sees the count"
        );
        assert_eq!(
            notes(&mut e, "carmeet.join_types").last(),
            Some(&vec![json!(2)])
        );
    }

    #[test]
    fn road_trip_invitations_are_asked_for_only_where_the_world_serves_them() {
        // the reference (and every player-hosted car meet): free roam only
        assert_eq!(asked(false, INVITE_FREE_ROAM), vec![INVITE_FREE_ROAM]);
        assert!(asked(false, INVITE_ROAD_TRIP).is_empty());
        // OD-191: the official car meet lobby asks for Road Trip too; type 3 stays ignored
        assert_eq!(asked(true, INVITE_ROAD_TRIP), vec![INVITE_ROAD_TRIP]);
        assert_eq!(asked(true, INVITE_FREE_ROAM), vec![INVITE_FREE_ROAM]);
        assert!(asked(true, 3).is_empty());
    }
}
