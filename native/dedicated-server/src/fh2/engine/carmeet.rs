// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Car meet game-mode invitations (CarMeetSynced, router 11; contract §4.4). A player starts
//! Online Free Roam from the meet (26); the others get a 20 s countdown and join or cancel (27).
//! When it ends the engine asks its world for one shared session (`world.gameSession.request`)
//! for the starter and everybody who joined; the joiners are told to wait (29, wait), then where
//! to go (32, plus CarMeetSynced vars 16/17). Any failure, or the starter leaving, cancels it for
//! everybody (invite type 0); the players already waiting first get 29(none), which ends their
//! wait. Updates go out reliably, never re-broadcast on a timer.

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
pub const CAR_MEET_JOIN_MS: i64 = 20_000;
pub const CAR_MEET_CREATE_MS: i64 = 15_000;
pub const CAR_MEET_READY_MS: i64 = 30_000;
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
}

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
        self.car_meet.sequence = seq_next(self.car_meet.sequence);
        let update = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 11,
            key: ObjKey::own(0),
            ty: 0,
            sequence: self.car_meet.sequence,
            vars: variables,
        })));
        for s in self.synced_serials_except(None) {
            self.send_sequenced(s, vec![update.clone()]);
        }
    }

    fn invite_cleared() -> Bits {
        let mut b = Bits::new();
        b.push(0, 5).push(0, 2);
        b
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
        if kind != INVITE_FREE_ROAM {
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
    }

    /// 27: a player joins or leaves the running invitation.
    pub(super) fn join_or_cancel(&mut self, serial: u64, body: &Bits) {
        if self.car_meet.stage != InviteStage::Joining || self.car_meet.starter == Some(serial) {
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
    }

    pub(super) fn tick_car_meet(&mut self) {
        let now = self.now;
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
                self.car_meet_update(Self::invite_cleared());
            }
            _ => {}
        }
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
        self.car_meet_update(car_meet_vars(Some(&session), CAR_MEET_JOIN_MS));
        let ready = session_ready(name, template);
        let joined: Vec<u64> = self
            .car_meet
            .joined
            .iter()
            .copied()
            .filter(|s| self.alive(*s))
            .collect();
        for s in &joined {
            self.send_sequenced(*s, vec![ready.clone()]);
        }
        self.note("carmeet.ready", vec![json!(joined.len())]);
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
        self.car_meet_update(Self::invite_cleared());
        self.note("carmeet.cancelled", vec![json!(why)]);
    }

    pub(super) fn car_meet_peer_left(&mut self, serial: u64) {
        self.car_meet.joined.retain(|s| *s != serial);
    }
}
