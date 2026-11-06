// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The session leader of a private official world (WORLDS_OFFICIAL.md §12.8b, T-R3's
//! `r3-session-leadership.md`; T-L9). Only an engine its world made private runs any of this; a
//! public engine never reaches past the first `if`.
//!
//! What the game fixes (T-R3): the server alone writes `IsLeader` (PlayerData var 5) and every
//! member reads it; a session whose `PrivacyMode` (SessionHostSynced var 1) is InviteOnly lets only
//! its leader start events and hand leadership on (RequestLeaderChange, message 59, carrying the
//! target's player key); DropLeadership (60) is sent in public sessions only, so it is not read.
//! What the game leaves to us (the owner's A3): the leader is the longest-present synced player,
//! picked again when the leader leaves. Every member is told, the old leader cleared first, so
//! no client ever sees two leaders.

use std::sync::Arc;

use serde_json::json;

use super::{
    Engine, Why,
    bits::Bits,
    key::{KeyOwner, ObjKey},
    message::{Message, ObjectChange},
};
use crate::fh2::codec::object::{seq_forward, seq_next};

/// SessionHostSynced var 1 `PrivacyMode` (2 bits): 0 Public, 1 InviteOnly (T-R3 §3).
const PRIVACY_MODE_VARIABLE: u8 = 1;
const PRIVACY_MODE_BITS: u32 = 2;
pub(super) const INVITE_ONLY: u64 = 1;
/// The player object's own `IsLeader` (PlayerData var 5) and LobbyPlayer.PlayerHost (var 13),
/// as Road Trip has always written them to its leader (`roadtrip.rs`).
pub(super) const IS_LEADER_VARIABLE: u64 = 5;
pub(super) const PLAYER_HOST_VARIABLE: u64 = 13;
/// What the leader flag adds to a relayed player object (IsLeader: an 8-bit id and 1 bit), on
/// top of the server variables every snapshot may add.
pub(super) const LEADER_SNAPSHOT_BITS: usize = 9;
/// RequestLeaderChange's target key: a 7-bit object id (the player router's layout).
const TARGET_KEY_BITS: u32 = 7;

impl Engine {
    /// The world became a private official world (one way): `PrivacyMode` InviteOnly to every
    /// member and every later joiner, and a leader from now on.
    pub fn set_private(&mut self) {
        if self.private {
            return;
        }
        self.private = true;
        self.set_host_variables(&[(PRIVACY_MODE_VARIABLE, INVITE_ONLY, PRIVACY_MODE_BITS)]);
        self.note("session.private", Vec::new());
        self.choose_leader();
    }

    pub fn is_private(&self) -> bool {
        self.private
    }

    /// The private session's leader's XUID (the agent announces it); `None` in a public engine
    /// or while nobody leads.
    pub fn leader_xuid(&self) -> Option<u64> {
        let leader = self.leader?;
        self.peers
            .iter()
            .find(|p| p.index == leader && p.synced)
            .map(|p| p.xuid)
    }

    /// Keeps a leader while anybody is in: the current one while it is synced, else the seat
    /// that took over its game once that seat is synced, else the longest-present synced player
    /// (earliest `joined_at`, the earlier seat on a tie). A doomed player is never picked.
    pub(super) fn choose_leader(&mut self) {
        if !self.private {
            return;
        }
        if let Some(leader) = self.leader
            && self.peers.iter().any(|p| p.index == leader && p.synced)
        {
            return;
        }
        if let Some(heir) = self.leader_heir {
            match self.peers.iter().find(|p| p.index == heir && !p.doomed) {
                Some(p) if p.synced => {
                    let serial = p.serial;
                    self.leader_heir = None;
                    self.assign_leader(Some(serial));
                    return;
                }
                // its game is still joining: leadership waits for it (within the same
                // OpenConnection's handshake; a seat that never syncs is dropped and clears this)
                Some(_) => return,
                None => self.leader_heir = None,
            }
        }
        let next = self
            .peers
            .iter()
            .filter(|p| p.synced && !p.doomed)
            .min_by_key(|p| (p.joined_at, p.serial))
            .map(|p| p.serial);
        self.assign_leader(next);
    }

    /// Moves leadership to `next` (a serial; `None`: nobody): the old leader, when still here,
    /// is cleared first, then the new one is set, each to every synced member.
    fn assign_leader(&mut self, next: Option<u64>) {
        let new = next.and_then(|s| self.peer(s)).map(|p| p.index);
        let old = self.leader;
        if old == new {
            return;
        }
        if let Some(o) = old
            && let Some(serial) = self.peers.iter().find(|p| p.index == o).map(|p| p.serial)
        {
            self.write_leader(serial, false);
        }
        self.leader = new;
        if let Some(t) = self.road_trip.as_mut() {
            t.set_leader(new);
        }
        if let Some(s) = next {
            self.write_leader(s, true);
        }
        self.note("session.leader", vec![json!(old), json!(new)]);
        self.road_trip_leader_changed();
    }

    /// The leader flag of a member: to it on its own object (IsLeader and PlayerHost, as Road
    /// Trip writes its leader), and IsLeader on its relayed object to every other synced member,
    /// kept for later joiners' snapshots. Nothing before its player object exists (its creation
    /// sends it then, [`Engine::leader_object_created`]).
    fn write_leader(&mut self, serial: u64, value: bool) {
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        if !p.synced {
            return;
        }
        let Some(id) = p.objects.iter().find(|(k, _)| k.0 == 1).map(|(k, _)| k.1) else {
            return;
        };
        let index = p.index;
        let bit = u64::from(value);
        let seq = p.player_sequence.map_or(0, seq_next);
        p.player_sequence = Some(seq);
        let mut own = Bits::new();
        own.push(IS_LEADER_VARIABLE, 8)
            .push(bit, 1)
            .push(PLAYER_HOST_VARIABLE, 8)
            .push(bit, 1);
        let own = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 1,
            key: ObjKey::server(id),
            ty: 0,
            sequence: seq,
            vars: own,
        })));
        self.send_sequenced(serial, vec![own]);
        let Some(obj) = self
            .peer_mut(serial)
            .and_then(|p| p.objects.get_mut(&(1, id)))
        else {
            return;
        };
        obj.injected = seq_next(obj.injected);
        obj.leader = value;
        let seq = seq_forward(obj.last, obj.injected);
        let mut relayed = Bits::new();
        relayed.push(IS_LEADER_VARIABLE, 8).push(bit, 1);
        let m = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 1,
            key: ObjKey::relay(id, index),
            ty: 0,
            sequence: seq,
            vars: relayed,
        })));
        // the server's own write: no sender's fan-out budget is charged here
        for r in self.synced_serials_except(Some(serial)) {
            self.send_sequenced(r, vec![m.clone()]);
        }
    }

    /// A member's player object was (re)created: the leader learns it leads now.
    pub(super) fn leader_object_created(&mut self, serial: u64) {
        if !self.private {
            return;
        }
        if self.leader.is_some() && self.leader == self.peer(serial).map(|p| p.index) {
            self.write_leader(serial, true);
        }
    }

    /// A member joined (synced): a session nobody leads gets its leader.
    pub(super) fn leader_joined(&mut self) {
        self.choose_leader();
    }

    /// A seat left (synced or not). The leader's seat: its objects are already deleted for
    /// everyone, so nothing is cleared; a seat that took over its game (a rebind) inherits it
    /// once synced; otherwise the longest-present member leads.
    pub(super) fn leader_peer_left(&mut self, index: u8, why: Why) {
        if !self.private {
            return;
        }
        if self.leader_heir == Some(index) {
            self.leader_heir = None;
        }
        if self.leader == Some(index) {
            self.leader = None;
            if let Some(t) = self.road_trip.as_mut() {
                t.set_leader(None);
            }
            if let Why::TakenOver(heir) = why {
                self.leader_heir = Some(heir);
            }
        }
        self.choose_leader();
    }

    /// RequestLeaderChange (59) in a private session: the current leader names a synced member by
    /// its player key (the relay key the sender sees). Anything else changes nothing.
    pub(super) fn request_leader_change(&mut self, serial: u64, body: &Bits) {
        let Some(index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        if self.leader != Some(index) {
            self.peer_noted(serial, "session.leader_refused", vec![json!(index)]);
            return;
        }
        let mut r = body.reader();
        let Ok(key) = ObjKey::read(&mut r, TARGET_KEY_BITS) else {
            return;
        };
        let target = self
            .peers
            .iter()
            .find(|p| {
                key.kind == KeyOwner::Relay
                    && p.index == key.peer
                    && p.index != index
                    && p.synced
                    && !p.doomed
                    && p.objects.contains_key(&(1, key.id))
            })
            .map(|p| p.serial);
        let Some(target) = target else {
            self.peer_noted(serial, "session.leader_refused", vec![json!(index)]);
            return;
        };
        // the clear and the set reach every other member: the asker's own traffic pays for them
        if !self.charge_fanout(serial, 2) {
            return;
        }
        self.assign_leader(Some(target));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::{Event, Why, bits::Bits, history::History, testkit},
        *,
    };
    use crate::fh2::engine::message::Message;

    const X: u64 = 1_000_000_000_000_100;

    /// A synced member with its player object (router 1, id 1) and console.
    fn member(e: &mut Engine, index: u8, joined_at: i64) -> u64 {
        let s = testkit::seat(e, index, X + u64::from(index));
        let p = e.peer_mut(s).unwrap();
        p.joined_at = joined_at;
        p.objects.insert((1, 1), History::new(0, 0, Bits::new(), 1));
        s
    }

    /// Every object change sent to `serial` so far (in flight or queued), in order; cleared.
    fn sent(e: &mut Engine, serial: u64) -> Vec<ObjectChange> {
        let p = e.peer_mut(serial).unwrap();
        let mut all: Vec<Arc<Bits>> = p.in_flight.drain(..).flat_map(|r| r.messages).collect();
        all.extend(p.queue.drain(..));
        p.queue_bits = 0;
        all.iter()
            .filter_map(|m| match Message::decode(&mut m.reader()).ok()? {
                Message::Object(o) => Some(o),
                _ => None,
            })
            .collect()
    }

    /// `(router, key, IsLeader)` of each player-object leader write in `changes`.
    fn leader_writes(changes: &[ObjectChange]) -> Vec<(ObjKey, bool)> {
        changes
            .iter()
            .filter(|o| o.router_id == 1 && !o.is_create)
            .filter_map(|o| {
                let mut r = o.vars.reader();
                (r.read(8).ok()? == IS_LEADER_VARIABLE).then_some(())?;
                Some((o.key, r.read(1).ok()? == 1))
            })
            .collect()
    }

    fn notes(e: &mut Engine) -> Vec<(&'static str, Vec<serde_json::Value>)> {
        e.take_events()
            .into_iter()
            .filter_map(|ev| match ev {
                Event::Note { id, args } => Some((id, args)),
                _ => None,
            })
            .collect()
    }

    fn private_engine() -> Engine {
        let mut e = testkit::engine();
        e.set_private();
        e
    }

    #[test]
    fn a_public_engine_has_no_leader_and_sends_no_privacy_mode() {
        let mut e = testkit::engine();
        let a = member(&mut e, 1, 10);
        e.leader_joined();
        e.leader_object_created(a);
        e.leader_peer_left(1, Why::Closed);
        assert!(!e.is_private());
        assert_eq!(e.leader, None);
        assert_eq!(e.leader_xuid(), None);
        assert!(sent(&mut e, a).is_empty());
        assert!(!e.host_variables.contains_key(&PRIVACY_MODE_VARIABLE));
    }

    #[test]
    fn privacy_mode_is_invite_only_for_members_and_later_joiners() {
        let mut e = testkit::engine();
        let a = member(&mut e, 1, 10);
        e.set_private();
        assert_eq!(
            e.host_variables.get(&PRIVACY_MODE_VARIABLE),
            Some(&(INVITE_ONLY, PRIVACY_MODE_BITS)),
            "kept for every later joiner's snapshot"
        );
        let to_a = sent(&mut e, a);
        let host = to_a
            .iter()
            .find(|o| o.router_id == 6 && !o.is_create)
            .expect("a SessionHostSynced update");
        let mut r = host.vars.reader();
        assert_eq!(r.read(4).unwrap(), u64::from(PRIVACY_MODE_VARIABLE));
        assert_eq!(r.read(2).unwrap(), INVITE_ONLY);
        // one way and idempotent: a second call sends nothing more
        e.set_private();
        assert!(sent(&mut e, a).iter().all(|o| o.router_id != 6));
    }

    #[test]
    fn the_longest_present_synced_member_leads_whatever_its_index() {
        let mut e = testkit::engine();
        let late = member(&mut e, 1, 500);
        let early = member(&mut e, 2, 100);
        let joining = member(&mut e, 3, 50);
        e.peer_mut(joining).unwrap().synced = false;
        e.set_private();
        assert_eq!(e.leader, Some(2), "earliest joined_at among the synced");
        assert_eq!(e.leader_xuid(), Some(X + 2));
        assert_eq!(
            leader_writes(&sent(&mut e, early)),
            vec![(ObjKey::server(1), true)]
        );
        assert_eq!(
            leader_writes(&sent(&mut e, late)),
            vec![(ObjKey::relay(1, 2), true)],
            "every member sees the crown"
        );
        assert!(sent(&mut e, joining).is_empty(), "not synced: nothing yet");
    }

    #[test]
    fn a_new_member_does_not_take_leadership() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        e.leader_joined();
        assert_eq!(e.leader, Some(1));
        let b = member(&mut e, 2, 20);
        e.leader_joined();
        assert_eq!(e.leader, Some(1));
        assert!(leader_writes(&sent(&mut e, b)).is_empty());
        let _ = sent(&mut e, a);
    }

    #[test]
    fn the_leaders_leave_picks_the_longest_present_and_tells_everyone() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        let b = member(&mut e, 2, 30);
        let c = member(&mut e, 3, 20);
        e.leader_joined();
        assert_eq!(e.leader, Some(1));
        for s in [a, b, c] {
            let _ = sent(&mut e, s);
        }
        let _ = notes(&mut e);
        e.drop_peer(a, Why::Closed);
        assert_eq!(e.leader, Some(3), "c joined before b");
        assert_eq!(
            leader_writes(&sent(&mut e, c)),
            vec![(ObjKey::server(1), true)]
        );
        let to_b = sent(&mut e, b);
        // the leaver's objects are deleted first; no clear is sent for a seat that is gone
        assert!(to_b.is_empty() || to_b.iter().all(|o| o.router_id == 1));
        assert_eq!(leader_writes(&to_b), vec![(ObjKey::relay(1, 3), true)]);
        assert!(
            notes(&mut e)
                .iter()
                .any(|(id, args)| *id == "session.leader" && args == &vec![json!(null), json!(3)]),
            "the gone leader is no longer named: nobody, then 3"
        );
    }

    #[test]
    fn a_non_leader_leaving_changes_nothing() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        let b = member(&mut e, 2, 20);
        e.leader_joined();
        let _ = (sent(&mut e, a), sent(&mut e, b));
        e.drop_peer(b, Why::Closed);
        assert_eq!(e.leader, Some(1));
        assert!(leader_writes(&sent(&mut e, a)).is_empty());
    }

    #[test]
    fn the_last_member_leaving_leaves_nobody_leading() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        e.leader_joined();
        e.drop_peer(a, Why::Closed);
        assert_eq!(e.leader, None);
        assert_eq!(e.leader_xuid(), None);
        // the next one in leads
        let b = member(&mut e, 2, 99);
        e.leader_joined();
        assert_eq!(e.leader, Some(2));
        assert_eq!(
            leader_writes(&sent(&mut e, b)),
            vec![(ObjKey::server(1), true)]
        );
    }

    #[test]
    fn a_leader_whose_game_rebinds_keeps_leading_through_the_new_seat() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        let b = member(&mut e, 2, 20);
        e.leader_joined();
        let _ = (sent(&mut e, a), sent(&mut e, b));
        // the new seat of a's game is handshaking when the old one is taken over
        let a2 = testkit::seat(&mut e, 3, X + 1);
        e.peer_mut(a2).unwrap().synced = false;
        e.drop_peer(a, Why::TakenOver(3));
        assert_eq!(e.leader, None, "nobody, for the rest of this handshake");
        assert!(
            leader_writes(&sent(&mut e, b)).is_empty(),
            "b is not made leader"
        );
        // it syncs; its player object comes later
        let p = e.peer_mut(a2).unwrap();
        p.synced = true;
        p.joined_at = 40;
        e.leader_joined();
        assert_eq!(e.leader, Some(3));
        e.peer_mut(a2)
            .unwrap()
            .objects
            .insert((1, 4), History::new(0, 0, Bits::new(), 1));
        e.leader_object_created(a2);
        assert_eq!(
            leader_writes(&sent(&mut e, a2)),
            vec![(ObjKey::server(4), true)]
        );
        assert_eq!(
            leader_writes(&sent(&mut e, b)),
            vec![(ObjKey::relay(4, 3), true)]
        );
    }

    #[test]
    fn a_takeover_seat_that_never_syncs_hands_leadership_on() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        let b = member(&mut e, 2, 20);
        e.leader_joined();
        let a2 = testkit::seat(&mut e, 3, X + 1);
        e.peer_mut(a2).unwrap().synced = false;
        e.drop_peer(a, Why::TakenOver(3));
        let _ = sent(&mut e, b);
        e.drop_peer(a2, Why::WorldFull);
        assert_eq!(e.leader, Some(2));
        assert_eq!(e.leader_heir, None);
        assert_eq!(
            leader_writes(&sent(&mut e, b)),
            vec![(ObjKey::server(1), true)]
        );
    }

    /// RequestLeaderChange's body: the target's relay key (7-bit id).
    fn request(target_id: u32, target_index: u8) -> Bits {
        let mut b = Bits::new();
        ObjKey::relay(target_id, target_index).write(&mut b, TARGET_KEY_BITS);
        b
    }

    #[test]
    fn the_leaders_request_moves_leadership_clearing_the_old_leader_first() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        let b = member(&mut e, 2, 20);
        let c = member(&mut e, 3, 30);
        e.leader_joined();
        for s in [a, b, c] {
            let _ = sent(&mut e, s);
        }
        e.request_leader_change(a, &request(1, 2));
        assert_eq!(e.leader, Some(2));
        assert_eq!(e.leader_xuid(), Some(X + 2));
        assert_eq!(
            leader_writes(&sent(&mut e, c)),
            vec![(ObjKey::relay(1, 1), false), (ObjKey::relay(1, 2), true)],
            "cleared, then set: never two leaders"
        );
        assert_eq!(
            leader_writes(&sent(&mut e, a)),
            vec![(ObjKey::server(1), false), (ObjKey::relay(1, 2), true)]
        );
        assert_eq!(
            leader_writes(&sent(&mut e, b)),
            vec![(ObjKey::relay(1, 1), false), (ObjKey::server(1), true)]
        );
        // a later joiner's snapshot carries only the new leader's flag
        assert!(!e.peer(a).unwrap().objects.get(&(1, 1)).unwrap().leader);
        assert!(e.peer(b).unwrap().objects.get(&(1, 1)).unwrap().leader);
    }

    #[test]
    fn requests_from_a_non_leader_or_for_an_unusable_target_change_nothing() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        let b = member(&mut e, 2, 20);
        let c = member(&mut e, 3, 30);
        e.peer_mut(c).unwrap().synced = false;
        e.leader_joined();
        for s in [a, b] {
            let _ = sent(&mut e, s);
        }
        e.request_leader_change(b, &request(1, 2)); // not the leader (asks for itself)
        e.request_leader_change(b, &request(1, 1)); // not the leader
        e.request_leader_change(a, &request(1, 3)); // not synced
        e.request_leader_change(a, &request(9, 2)); // not b's player object
        e.request_leader_change(a, &request(1, 1)); // itself
        e.request_leader_change(a, &request(1, 7)); // nobody
        let mut own = Bits::new();
        ObjKey::own(1).write(&mut own, TARGET_KEY_BITS);
        e.request_leader_change(a, &own); // not a relay key
        e.request_leader_change(a, &Bits::new()); // truncated
        assert_eq!(e.leader, Some(1));
        assert!(leader_writes(&sent(&mut e, a)).is_empty());
        assert!(leader_writes(&sent(&mut e, b)).is_empty());
    }

    #[test]
    fn the_leaders_snapshot_flag_follows_its_writes() {
        let mut e = private_engine();
        let a = member(&mut e, 1, 10);
        e.leader_joined();
        let obj = e.peer_mut(a).unwrap().objects.get_mut(&(1, 1)).unwrap();
        assert!(obj.leader);
        let mut notes = Vec::new();
        let snapshot = obj.snapshot(1, ObjKey::relay(1, 1), &mut notes);
        let Message::Object(create) = Message::decode(&mut snapshot[0].reader()).unwrap() else {
            panic!("a create");
        };
        let mut r = create.vars.reader();
        assert_eq!(r.read(8).unwrap(), IS_LEADER_VARIABLE);
        assert_eq!(r.read(1).unwrap(), 1);
        assert_eq!(r.left(), 0);
    }
}
