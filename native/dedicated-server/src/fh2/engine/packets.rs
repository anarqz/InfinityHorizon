// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! A packet in: the connection handshake (OpenConnection, the address proof, SyncDone and its
//! confirmation), the messages a synced game sends (objects, deletes, car predictions and events,
//! group navigation, the finalise), the relayed objects' histories, and the handshake's completion
//! with the joiner's snapshot.

use std::sync::Arc;

use serde_json::json;

use super::{
    Engine, Event, M, Open, Why,
    bits::{Rd, Rr},
    datagram::{self, UNSEQUENCED},
    history::History,
    key::{KeyOwner, ObjKey},
    message::{self, CAR_BODY_BITS, Message, ObjectChange, SAT_NAV_NODE_MAX},
    payload,
};
use crate::fh2::codec::object::{seq_forward, seq_is_newer, seq_next};

/// Most objects a peer keeps (its creates past it are relayed but not kept for late joiners).
const MAX_OBJECTS: usize = 256;
/// Updates kept for objects whose create has not come yet.
const MAX_ORPHANS: usize = 32;
/// A Road Trip player object's largest payload (router 9, type 2).
/// The bucket-list router and its player state type.
pub(super) const BUCKET_LIST_ROUTER: u8 = 12;
const BUCKET_LIST_PLAYER_TYPE: u8 = 0;
pub(super) const BUCKET_LIST_SLOTS_TYPE: u8 = 1;
/// Before its handshake completed, a peer's packet id may move at most this far forward.
const PRE_AUTH_MAX_JUMP: u16 = 0x1000;

/// What a snapshot adds to a player object (PrivateUniverseId 8+5, TeamId 8+1) and to a
/// console (nine server flags, 4+1 each).
const SERVER_PLAYER_BITS: usize = 13 + 9;
const SERVER_CONSOLE_BITS: usize = 9 * 5;
/// The time stamp's offset in a car prediction's body (its last 32 bits).
pub(super) const CAR_STAMP_OFFSET: usize = CAR_BODY_BITS - 32;

impl Engine {
    pub(super) fn receive_packet(&mut self, data: &[u8], endpoint: std::net::SocketAddr) -> Rr<()> {
        let Some(header) = datagram::header_of(data) else {
            return Ok(());
        };
        let body = datagram::messages_of(data);
        let mut reader = Rd::new(body);
        let first = if header.count > 0 {
            let mut probe = reader;
            Message::decode(&mut probe).ok()
        } else {
            None
        };
        let first_open = first.as_ref().and_then(Open::of);
        if let Some(o) = &first_open
            && self.retired_attempt(endpoint, o.attempt)
        {
            return Ok(());
        }
        let mut serial = self
            .peers
            .iter()
            .find(|p| p.endpoint == endpoint)
            .map(|p| p.serial);
        if let (Some(s), true, Some(restart)) = (serial, header.id == 0, first_open.as_ref()) {
            let Some(p) = self.peer(s) else {
                return Ok(());
            };
            if restart.user != p.xuid
                || restart.version != message::PROTOCOL_VERSION
                || restart.game != message::GAME_TITLE
            {
                return Ok(());
            }
            if restart.attempt != p.client_attempt {
                self.retire_attempt(s);
                self.drop_peer(s, Why::Reconnected);
                serial = None;
            }
        }
        let mut admitted = false;
        let serial = match serial {
            Some(s) => s,
            None => {
                let Some(open) = &first_open else {
                    return Ok(());
                };
                let Some(s) = self.admit(endpoint, header, open, data) else {
                    return Ok(());
                };
                admitted = true;
                s
            }
        };
        let xuid = self.peer(serial).map_or(0, |p| p.xuid);
        if !self.allowed(xuid, endpoint) {
            self.drop_peer(serial, Why::NotAllowed);
            return Ok(());
        }
        if !self.take_rate(serial, header, data.len()) {
            return Ok(());
        }
        let now = self.now;
        let resend = self.settings.relay.resend_after_ms as i64;
        let Some(p) = self.peer_mut(serial) else {
            return Ok(());
        };
        p.last_seen = now;
        if !p.synced {
            p.received_bytes += data.len() as i64;
        }
        p.in_flight
            .retain(|x| !datagram::acknowledges(header.ack, header.ack_bits, x.id));
        if !admitted
            && header.id == 0
            && let Some(repeated) = &first_open
            && repeated.attempt == p.client_attempt
        {
            let own = p.attempt;
            if self.handshake_allowed(endpoint, xuid) {
                let m = Arc::new(Message::encode(&Message::ConnectReply {
                    flags: repeated.flags,
                    reason: 0,
                    attempt: repeated.attempt,
                    own_attempt: own,
                }));
                self.send(serial, vec![m], false, 0);
            }
            return Ok(());
        }
        // packet-id hardening (MACBOOK decision 3): until its handshake completed, a peer's id may
        // not jump more than 0x1000 forward (no ack, nothing applied); a synced peer, admitted by
        // the verified grant and endpoint binding, gets the game's own window
        if !p.synced
            && header.id != datagram::UNSEQUENCED
            && let Some(newest) = p.acks.newest()
        {
            let jump = crate::fh2::codec::reliable::id_dist(header.id, newest);
            if jump > PRE_AUTH_MAX_JUMP && jump <= crate::fh2::codec::reliable::HALF_SPACE {
                let index = p.index;
                if self.peer_note(serial, "id_jump") {
                    self.note("receive.id_jump_dropped", vec![json!(index), json!(jump)]);
                }
                return Ok(());
            }
        }
        let Some(p) = self.peer_mut(serial) else {
            return Ok(());
        };
        let verdict = p.acks.accept(header.id);
        // a dropped (0x3fff or more from the newest) or stale id is not recorded, never applied,
        // and answered with the acknowledgement as it stands, as the reference engine answers it
        if verdict != datagram::Verdict::Fresh {
            if let Some(Message::ConnectReply {
                reason: 6, attempt, ..
            }) = &first
                && p.synced
                && !p.completion_confirmed
                && *attempt == p.attempt
                && now - p.last_completion >= resend
            {
                self.send_completion(serial);
            }
            self.send_ack(serial, Vec::new());
            return Ok(());
        }
        let mut answers: Vec<M> = Vec::new();
        let mut reliable: Vec<M> = Vec::new();
        let mut forward: Vec<M> = Vec::new();
        let mut cars: Vec<M> = Vec::new();
        let mut synced_now = false;
        let mut early_finalise = false;
        let mut ready_release = false;
        let mut car_now: Option<(ObjKey, crate::fh2::engine::bits::Bits)> = None;
        let synced = p.synced;
        let (index, peer_attempt) = (p.index, p.attempt);
        let races = self.settings.races;
        let mut i = 0u32;
        while i < u32::from(header.count) && reader.left() >= 7 {
            i += 1;
            let message = match Message::decode(&mut reader) {
                Ok(m) => m,
                Err(_) => {
                    self.rejected(xuid, "truncated_packet", None, None, None);
                    break;
                }
            };
            match message {
                Message::Connect {
                    user,
                    flags,
                    attempt,
                    ..
                } => {
                    if user != xuid {
                        continue;
                    }
                    if !admitted && !self.handshake_allowed(endpoint, xuid) {
                        continue;
                    }
                    admitted = false;
                    answers.push(Arc::new(Message::encode(&Message::ConnectReply {
                        flags,
                        reason: 0,
                        attempt,
                        own_attempt: peer_attempt,
                    })));
                }
                Message::ConnectReply {
                    reason, attempt, ..
                } => {
                    let resend = self.settings.relay.resend_after_ms as i64;
                    let Some(p) = self.peer(serial) else { break };
                    if !synced && reason == 6 && attempt == peer_attempt {
                        synced_now = true;
                    } else if synced
                        && !p.completion_confirmed
                        && reason == 6
                        && attempt == peer_attempt
                        && now - p.last_completion >= resend
                    {
                        self.send_completion(serial);
                    }
                }
                Message::Disconnect => {
                    self.drop_peer(serial, Why::Closed);
                    return Ok(());
                }
                Message::Ping { clock: time } => {
                    let wire_now = self.now_wire();
                    answers.push(Arc::new(Message::encode(&Message::Pong {
                        echoed: time,
                        clock: wire_now,
                    })));
                }
                Message::Pong {
                    echoed: echo,
                    clock: time,
                } => {
                    let wire_now = self.now_wire();
                    if echo == 0
                        && time <= wire_now
                        && wire_now - time < 60_000
                        && let Some(p) = self.peer_mut(serial)
                    {
                        p.rtt = Some(wire_now - time);
                    }
                }
                // a finalise in the packet that proves the address: applied once synced (below)
                Message::Finalise if !synced => early_finalise = true,
                _ if !synced => {}
                Message::Object(obj) => {
                    if let Some(relayed) = self.on_object(serial, obj) {
                        if let Some(p) = self.peer_mut(serial) {
                            p.completion_confirmed = true;
                        }
                        forward.push(relayed);
                    }
                }
                Message::ObjectDelete {
                    router_id: router,
                    key: handle,
                    sequence: seq,
                } if handle.kind == KeyOwner::Own
                    && ((1..=3).contains(&router)
                        || router == BUCKET_LIST_ROUTER
                            && handle.id == 0
                            && self.bucket_list_world()) =>
                {
                    if router == BUCKET_LIST_ROUTER {
                        self.bucket_list_player_deleted(serial);
                    }
                    let Some(p) = self.peer_mut(serial) else {
                        break;
                    };
                    let removed = p.objects.remove(&(router, handle.id));
                    p.orphans
                        .retain(|o| o.router_id != router || o.key.id != handle.id);
                    let injected = removed.map_or(0, |o| o.injected);
                    forward.push(Arc::new(Message::encode(&Message::ObjectDelete {
                        router_id: router,
                        key: ObjKey::relay(handle.id, index),
                        sequence: seq_forward(seq, injected),
                    })));
                }
                Message::CarState { car: handle, body } if handle.kind == KeyOwner::Own => {
                    match payload::vet_car_body(15, &body) {
                        Some([x, y, z, ..]) => {
                            let position = [x, y, z];
                            // voice proximity's jump counter, on the previous car
                            self.car_accepted(serial, handle.id, position);
                            let Some(p) = self.peer_mut(serial) else {
                                break;
                            };
                            p.completion_confirmed = true;
                            p.last_car_at = Some(now);
                            p.last_car_body = Some(body.clone());
                            p.car_position = Some(position);
                            p.car_id = Some(handle.id);
                            car_now = Some((ObjKey::relay(handle.id, index), body));
                        }
                        None => self.rejected(xuid, "invalid_car", Some(15), None, None),
                    }
                }
                Message::CarEvent {
                    id: kind,
                    car: handle,
                    body,
                } if handle.kind == KeyOwner::Own => {
                    if payload::vet_car_body(kind, &body).is_some() {
                        let change = Arc::new(Message::encode(&Message::CarEvent {
                            id: kind,
                            car: ObjKey::relay(handle.id, index),
                            body,
                        }));
                        if kind == 35 {
                            forward.push(change);
                        } else {
                            cars.push(change);
                        }
                    } else {
                        self.rejected(xuid, "invalid_car_event", Some(u32::from(kind)), None, None);
                    }
                }
                Message::GroupSatNav { node, .. } if (0..=SAT_NAV_NODE_MAX).contains(&node) => {
                    if let Some(p) = self.peer_mut(serial) {
                        p.completion_confirmed = true;
                    }
                    self.set_group_node(node);
                }
                Message::Opaque { id: kind, body } if self.other_seam(serial, kind, &body) => {}
                Message::Opaque { id: kind, body } if races => {
                    // a Ready ends the join's hold after this packet's own answers (an ownership
                    // refusal's sequence precedes the options replay), before any race step
                    let (answers_of, release) = self.race_message(serial, kind, &body);
                    reliable.extend(answers_of);
                    ready_release |= release;
                }
                Message::TagCollision {
                    first: a,
                    second: b,
                    values,
                } if races => self.tag_collision(serial, a, b, values),
                Message::Finalise => {
                    if let Some(p) = self.peer_mut(serial) {
                        p.completion_confirmed = true;
                        p.finalise_asked = true;
                    }
                }
                // server-only, traffic, chat and other layouts are read but never relayed
                _ => {}
            }
        }
        // T38: a RaceInProgress 1 -> 0 is decided after the packet's own 45
        self.resolve_race_exits();
        let max_bits = self.settings.relay.history_max_bits as usize;
        let max_entries = self.settings.relay.history_max_entries as usize;
        let Some(p) = self.peer(serial) else {
            return Ok(());
        };
        if p.history_bits() > max_bits {
            self.doom(serial, Why::HistoryBits);
            return Ok(());
        }
        // each kept entry costs its own buffer whatever its bits: the largest histories are trimmed
        // first; still over after that is a flood across many objects
        if p.history_entries() > max_entries && self.trim_history(serial, max_entries) > max_entries
        {
            self.doom(serial, Why::HistoryEntries);
            return Ok(());
        }
        // the join snapshot's share (contract §8): shed down to it, never a drop
        let share = self.settings.relay.history_trim_entries as usize;
        if self
            .peer(serial)
            .is_some_and(|p| p.history_entries() > share)
        {
            self.shed_history(serial, share);
        }
        // AC-2: a watching host measures the other players' cars
        if let Some((_, body)) = &car_now {
            self.watch_car(serial, body);
        }
        self.forward(serial, forward, true);
        self.forward_cars(serial, car_now, cars);
        // after the relayed objects: a PlayerState create precedes the 65 that names it
        self.flush_bucket_list();
        if let Some(p) = self.peer_mut(serial)
            && p.universe_due
        {
            p.universe_due = false;
            self.player_object_created(serial);
        }
        if let Some(f) = self.finalise(serial) {
            reliable.push(f);
        }
        if !reliable.is_empty() {
            self.send_sequenced(serial, reliable);
        }
        // after its console's var 1: the cars and car traffic held for this joiner
        if self.peer(serial).is_some_and(|p| p.finalise_asked) {
            self.release_joiner(serial, super::join::Release::Finalise);
        }
        if ready_release {
            self.release_joiner(serial, super::join::Release::Ready);
        }
        self.race_step();
        if !answers.is_empty() || header.id != UNSEQUENCED {
            self.send_ack(serial, answers);
        }
        if !synced_now {
            return Ok(());
        }
        self.complete_handshake(serial);
        if early_finalise && self.peer(serial).is_some_and(|p| p.synced) {
            if let Some(p) = self.peer_mut(serial) {
                p.completion_confirmed = true;
                p.finalise_asked = true;
            }
            self.release_joiner(serial, super::join::Release::Finalise);
        }
        Ok(())
    }

    /// Trims the peer's largest histories first until under `limit`; the entries left.
    fn trim_history(&mut self, serial: u64, limit: usize) -> usize {
        let Some(p) = self.peer_mut(serial) else {
            return 0;
        };
        let index = p.index;
        let mut entries = p.history_entries();
        let mut order: Vec<((u8, u32), usize)> = p
            .objects
            .iter()
            .map(|(k, o)| (*k, o.history_entries()))
            .collect();
        // largest first, ties in map order (a stable sort)
        order.sort_by_key(|x| std::cmp::Reverse(x.1));
        let mut guard_notes = Vec::new();
        for (key, _) in order {
            if entries <= limit {
                break;
            }
            if let Some(h) = p.objects.get_mut(&key) {
                let before = h.history_entries();
                let mut notes = Vec::new();
                h.trim(&mut notes);
                entries -= before - h.history_entries();
                guard_notes.extend(notes.into_iter().map(|d| (key, d)));
            }
        }
        for (key, d) in guard_notes {
            self.guard_note(index, key, d);
        }
        entries
    }

    /// Sheds the peer's oldest kept updates, largest histories first, until its entries are
    /// within the join snapshot's share `limit` (contract §8), a level compaction and the
    /// per-object guard need not reach; the entries left (more than `limit` only when its
    /// creates alone exceed it).
    fn shed_history(&mut self, serial: u64, limit: usize) -> usize {
        let Some(p) = self.peer_mut(serial) else {
            return 0;
        };
        let index = p.index;
        let mut entries = p.history_entries();
        let mut order: Vec<((u8, u32), usize)> =
            p.objects.iter().map(|(k, o)| (*k, o.tail.len())).collect();
        order.sort_by_key(|x| std::cmp::Reverse(x.1));
        let mut guard_notes = Vec::new();
        for (key, _) in &order {
            if entries <= limit {
                break;
            }
            if let Some(h) = p.objects.get_mut(key) {
                let keep = h.tail.len().saturating_sub(entries - limit);
                let mut notes = Vec::new();
                entries -= h.shed_to(keep, &mut notes);
                guard_notes.extend(notes.into_iter().map(|d| (*key, d)));
            }
        }
        for (key, d) in guard_notes {
            self.guard_note(index, key, d);
        }
        entries
    }

    pub(super) fn guard_note(&mut self, peer: u8, key: (u8, u32), discarded: usize) {
        self.note(
            "history.guard",
            vec![
                json!(peer),
                json!(key.0),
                json!(key.1),
                json!(super::history::MAX_KEPT_UPDATES),
                json!(super::history::MAX_KEPT_BITS),
                json!(discarded),
            ],
        );
    }

    /// One object message from a synced peer: sanitized, kept in its history, and returned as the
    /// copy the others get (its relay key, its sequence past what the server injected); `None`
    /// when it is not relayed.
    fn on_object(&mut self, serial: u64, mut obj: ObjectChange) -> Option<M> {
        let (xuid, index) = self.peer(serial).map(|p| (p.xuid, p.index))?;
        if self.settings.races && obj.router_id == 6 && obj.key.kind == KeyOwner::Transfer {
            // the event options journal: published by races.rs, never relayed as it came
            self.on_game_options(serial, &obj);
            return None;
        }
        let has = |e: &Engine, key: (u8, u32)| {
            e.peer(serial).is_some_and(|p| p.objects.contains_key(&key))
        };
        // the game makes its RoadTripPlayerSynced in every session type: relayed in every world
        let road_trip_player = obj.router_id == 9
            && obj.key.kind == KeyOwner::Own
            && if obj.is_create {
                obj.ty == 2
            } else {
                has(self, (9, obj.key.id))
            };
        // a player's own BucketListPlayerStateSynced (router 12, type 0, id 0) in free roam
        let bucket_player = obj.router_id == BUCKET_LIST_ROUTER
            && self.bucket_list_world()
            && obj.key.kind == KeyOwner::Own
            && obj.key.id == 0
            && if obj.is_create {
                obj.ty == BUCKET_LIST_PLAYER_TYPE
            } else {
                has(self, (BUCKET_LIST_ROUTER, 0))
            };
        if !road_trip_player {
            if !bucket_player
                && (obj.key.kind != KeyOwner::Own
                    || !(1..=3).contains(&obj.router_id)
                    || obj.ty != 0)
            {
                if obj.router_id == 9 {
                    self.note(
                        "roadtrip.object_refused",
                        vec![
                            json!(index),
                            json!(9),
                            json!(if obj.is_create { "True" } else { "False" }),
                            json!(obj.ty),
                            json!(format!("{:?}", obj.key.kind)),
                            json!(obj.key.id),
                            json!(obj.vars.len()),
                        ],
                    );
                }
                if obj.router_id == BUCKET_LIST_ROUTER {
                    self.bucket_list_refused(serial, &obj);
                }
                return None;
            }
            // a PlayerState the object store cannot keep is not relayed either
            if bucket_player
                && obj.is_create
                && !has(self, (BUCKET_LIST_ROUTER, 0))
                && self
                    .peer(serial)
                    .is_some_and(|p| p.objects.len() >= MAX_OBJECTS)
            {
                self.bucket_list_capped(serial);
                return None;
            }
            match payload::sanitize(obj.router_id, &obj.vars) {
                Ok(clean) => {
                    obj.vars = clean;
                    if obj.router_id == 1
                        && !obj.is_create
                        && obj.vars.len() >= super::roadtrip::CAR_CHOICE_BITS
                    {
                        self.road_trip_car_picked(serial);
                    }
                    if obj.router_id == 1 {
                        self.observe_car_choice(serial, &obj.vars);
                        // NR-6: a watching host checks the others' parts
                        self.watch_player_object(serial, obj.is_create, &obj.vars);
                    }
                }
                Err(refusal) => {
                    let message = if obj.is_create { 7 } else { 8 };
                    self.rejected(
                        xuid,
                        "invalid_payload",
                        Some(message),
                        Some(u32::from(obj.router_id)),
                        Some(refusal.detail),
                    );
                    // a bucket-list PlayerState fails open: its clean prefix, re-encoded
                    obj.vars = bucket_player
                        .then(|| self.bucket_list_sanitize_refused(serial, &obj, refusal))
                        .flatten()?;
                }
            }
        }
        if road_trip_player {
            // validated against its variable list (NERDCORE_WIRE.md §7.9), as routers 1-3 are
            match payload::sanitize(9, &obj.vars) {
                Ok(clean) => obj.vars = clean,
                Err(refusal) => {
                    let message = if obj.is_create { 7 } else { 8 };
                    self.rejected(
                        xuid,
                        "invalid_payload",
                        Some(message),
                        Some(9),
                        Some(refusal.detail),
                    );
                    if self.peer_note(serial, "roadtrip_player") {
                        self.note(
                            "roadtrip.player_refused",
                            vec![
                                json!(index),
                                json!(if obj.is_create { "create" } else { "update" }),
                                json!(obj.sequence),
                                json!(refusal.cause.label()),
                                json!(refusal.at),
                                json!(obj.vars.len()),
                            ],
                        );
                    }
                    return None;
                }
            }
            if obj.is_create && self.is_road_trip() {
                self.note(
                    "roadtrip.player_created",
                    vec![json!(index), json!(9), json!(2), json!(obj.key.id)],
                );
            }
            self.road_trip_player_update(serial, &obj.vars);
        }
        // ingest-size rule: a message whose relayed form (relay key, and the server variables a
        // snapshot adds) cannot fit one datagram is refused here, before it takes a sequence or
        // enters a history, never dropped later at send
        let server_bits = match obj.router_id {
            1 => SERVER_PLAYER_BITS,
            3 => SERVER_CONSOLE_BITS,
            _ => 0,
        };
        if obj.vars.len() + server_bits > datagram::MAX_DATAGRAM_PAYLOAD_BITS {
            let message = if obj.is_create { 7 } else { 8 };
            let bits = obj.vars.len() as u32;
            self.rejected(
                xuid,
                "invalid_payload",
                Some(message),
                Some(u32::from(obj.router_id)),
                Some(bits),
            );
            self.note(
                "object.oversize_refused",
                vec![json!(index), json!(obj.router_id), json!(bits)],
            );
            return None;
        }
        let key = (obj.router_id, obj.key.id);
        let mut guard_notes = Vec::new();
        let p = self.peer_mut(serial)?;
        if obj.is_create {
            if let Some(prev) = p.objects.get(&key)
                && !seq_is_newer(obj.sequence, prev.last)
            {
                return None;
            }
            p.objects.remove(&key);
            if p.objects.len() < MAX_OBJECTS {
                let mut history =
                    History::new(obj.ty, obj.sequence, obj.vars.clone(), obj.router_id);
                let orphans: Vec<ObjectChange> = p.orphans.drain(..).collect();
                for orphan in orphans {
                    if (orphan.router_id, orphan.key.id) == key {
                        history.update(orphan.sequence, orphan.vars, &mut guard_notes);
                    } else {
                        p.orphans.push_back(orphan);
                    }
                }
                p.objects.insert(key, history);
            }
            if obj.router_id == 1 {
                p.universe_due = true;
            }
            if obj.router_id == 3 && p.console_id != Some(obj.key.id) {
                p.console_id = Some(obj.key.id);
                p.console_sequence = None;
            }
            if obj.router_id == 3 {
                p.console_variable_sequences.clear();
                p.console_latest_sequence = Some(obj.sequence);
            }
        } else if let Some(prev) = p.objects.get_mut(&key) {
            if !prev.update(obj.sequence, obj.vars.clone(), &mut guard_notes) {
                for d in guard_notes {
                    self.guard_note(index, key, d);
                }
                return None;
            }
        } else {
            if p.orphans.iter().any(|o| {
                o.router_id == obj.router_id && o.key.id == obj.key.id && o.sequence == obj.sequence
            }) {
                return None;
            }
            if p.orphans.len() >= MAX_ORPHANS {
                p.orphans.pop_front();
            }
            p.orphans.push_back(obj.clone());
        }
        if obj.router_id == 3 && p.console_id == Some(obj.key.id) {
            if p.console_latest_sequence
                .is_none_or(|l| seq_is_newer(obj.sequence, l))
            {
                p.console_latest_sequence = Some(obj.sequence);
            }
            let mut observations = Vec::new();
            for (var, value) in payload::console_variables(&obj.vars).unwrap_or_default() {
                let newer = p
                    .console_variable_sequences
                    .get(&var)
                    .is_none_or(|last| seq_is_newer(obj.sequence, *last));
                if newer {
                    p.console_variable_sequences.insert(var, obj.sequence);
                    observations.push((var, value));
                }
            }
            let injected = p.objects.get(&key).map_or(0, |o| o.injected);
            for d in guard_notes.drain(..) {
                self.guard_note(index, key, d);
            }
            self.observe_console(serial, &observations);
            return Some(Arc::new(Message::encode(&Message::Object(ObjectChange {
                key: ObjKey::relay(obj.key.id, index),
                sequence: seq_forward(obj.sequence, injected),
                ..obj
            }))));
        }
        for d in guard_notes {
            self.guard_note(index, key, d);
        }
        if bucket_player {
            self.bucket_list_player_update(serial, obj.is_create, obj.sequence, &obj.vars);
        }
        if obj.router_id == 1 {
            self.bucket_list_player_object(serial, &obj.vars); // the name 65 gives a partner
        }
        let injected = self
            .peer(serial)
            .and_then(|p| p.objects.get(&key))
            .map_or(0, |o| o.injected);
        Some(Arc::new(Message::encode(&Message::Object(ObjectChange {
            key: ObjKey::relay(obj.key.id, index),
            sequence: seq_forward(obj.sequence, injected),
            ..obj
        }))))
    }

    /// The server's SessionEntryFinalised (console var 1) for a peer that asked, once.
    fn finalise(&mut self, serial: u64) -> Option<M> {
        let p = self.peer_mut(serial)?;
        if !p.finalise_asked || p.finalised {
            return None;
        }
        let id = p.console_id?;
        p.finalised = true;
        let seq = p.console_sequence.map_or(0, seq_next);
        p.console_sequence = Some(seq);
        Some(Arc::new(Message::encode(&Message::Object(
            payload::console_update(id, seq, &[(1, true)]),
        ))))
    }

    fn set_group_node(&mut self, node: i32) {
        if node == self.group_node {
            return;
        }
        self.group_node = node;
        self.host_sequence = seq_next(self.host_sequence);
        let update = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 6,
            key: ObjKey::own(0),
            ty: 0,
            sequence: self.host_sequence,
            vars: payload::group_sat_nav(node),
        })));
        for s in self.synced_serials_except(None) {
            self.send_sequenced(s, vec![update.clone()]);
        }
    }

    pub(super) fn tick_weather(&mut self) {
        if !self.peers.iter().any(|p| p.synced) {
            return;
        }
        let (now, tick) = (self.now, self.now_wire());
        if !self.weather.tick(now, tick) {
            return;
        }
        self.weather_sequence = seq_next(self.weather_sequence);
        let update = Arc::new(Message::encode(&Message::Object(ObjectChange {
            is_create: false,
            router_id: 10,
            key: ObjKey::own(0),
            ty: 0,
            sequence: self.weather_sequence,
            vars: self.weather.payload(),
        })));
        for s in self.synced_serials_except(None) {
            self.send_sequenced(s, vec![update.clone()]);
        }
        let w = &self.weather;
        let ramp: f64 = format!("{:.3}", w.atmosphere_desired)
            .parse()
            .unwrap_or(0.0);
        let args = vec![
            json!(w.level),
            number(ramp),
            json!(w.rain_desired.round() as i64),
        ];
        self.note("weather.changed", args);
    }

    /// The handshake's end: the peer is synced, gets its completion and its snapshot (the world's
    /// objects, then the players already here; their cars follow at its finalise).
    fn complete_handshake(&mut self, serial: u64) {
        let Some(p) = self.peer(serial) else { return };
        let (xuid, endpoint, index) = (p.xuid, p.endpoint, p.index);
        if !self.allowed(xuid, endpoint) {
            self.drop_peer(serial, Why::NotAllowedAtSync);
            return;
        }
        let old = if super::shared_xuid(xuid) {
            None
        } else {
            self.peers
                .iter()
                .find(|o| o.serial != serial && o.synced && o.xuid == xuid)
                .map(|o| (o.serial, o.last_seen, o.index, o.endpoint))
        };
        if let Some((old_serial, last_seen, old_index, old_endpoint)) = old {
            // a game that rebinds its socket comes back from the same address with a new port:
            // when the installed policy verified this endpoint for this identity (the host's
            // loopback, a player's FIUP binding), the old seat is that game's dead socket and is
            // taken over at once; otherwise the reference's quiet rule (TakeoverQuietMs) holds
            let rebind = self.policy.has_authorize()
                && crate::net::ip_key(old_endpoint) == crate::net::ip_key(endpoint);
            if !rebind && self.now - last_seen < self.settings.relay.takeover_quiet_ms as i64 {
                self.drop_peer(serial, Why::StillActive(old_index));
                return;
            }
            self.drop_peer(old_serial, Why::TakenOver(index));
        }
        if self.synced() >= self.settings.max_players {
            self.drop_peer(serial, Why::WorldFull);
            return;
        }
        let now = self.now;
        if let Some(p) = self.peer_mut(serial) {
            p.synced = true;
            p.joined_at = now;
        }
        self.events.push(Event::PeerSynced { xuid, peer: index });
        self.holding.insert(serial, now); // quiet join (join.rs)
        self.send_completion(serial);
        let host: Vec<(u8, u64, u32)> = self
            .host_variables
            .iter()
            .map(|(k, (v, w))| (*k, *v, *w))
            .collect();
        let mut host_payload = payload::session_host(&host);
        if self.group_node != 0 {
            host_payload.append(&payload::group_sat_nav(self.group_node));
        }
        let mut snapshot: Vec<crate::fh2::engine::bits::Bits> = vec![
            Message::encode(&Message::Object(ObjectChange {
                is_create: true,
                router_id: 6,
                key: ObjKey::own(0),
                ty: 0,
                sequence: self.host_sequence,
                vars: host_payload,
            })),
            // the world's shared weather at its current sequence
            Message::encode(&Message::Object(ObjectChange {
                is_create: true,
                router_id: 10,
                key: ObjKey::own(0),
                ty: 0,
                sequence: self.weather_sequence,
                vars: self.weather.payload(),
            })),
        ];
        self.append_car_meet_snapshot(&mut snapshot);
        self.append_road_trip_snapshot(serial, &mut snapshot);
        self.append_bucket_list_snapshot(&mut snapshot);
        // the players already here (routers 1, 3, 9 and 12); their cars follow at the finalise
        let owners: Vec<u64> = self.synced_serials_except(Some(serial));
        for o in owners {
            let mut guard = Vec::new();
            let Some(owner) = self.peer_mut(o) else {
                continue;
            };
            let owner_index = owner.index;
            for (key, obj) in owner.objects.iter_mut() {
                if key.0 == 2 {
                    continue;
                }
                let mut notes = Vec::new();
                snapshot.extend(obj.snapshot(key.0, ObjKey::relay(key.1, owner_index), &mut notes));
                guard.extend(notes.into_iter().map(|d| (*key, d)));
            }
            for (key, d) in guard {
                self.guard_note(owner_index, key, d);
            }
        }
        for m in snapshot {
            if !self.queue_reliable(serial, Arc::new(m)) {
                break;
            }
        }
        // during an event a newcomer needs a private universe of its own too (free-roam side)
        if self.ev.phasing {
            self.assign_universe(serial);
        }
        self.road_trip_joined(serial);
        self.pace(serial);
    }
}

/// A note number as the world log prints it: an integral value as an integer.
fn number(x: f64) -> serde_json::Value {
    if x.fract() == 0.0 && x.abs() < 1e15 {
        json!(x as i64)
    } else {
        json!(x)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        bits::Bits,
        history::History,
        testkit::{engine, seat},
    };
    use crate::fh2::codec::object::seq_next;

    /// One player object whose kept updates compaction cannot shrink: two variables each, so
    /// none supersedes another.
    fn long_history(updates: u32) -> History {
        let mut h = History::new(0, 0, Bits::new(), 1);
        let mut notes = Vec::new();
        let mut seq = 0u8;
        for i in 1..=updates {
            seq = seq_next(seq);
            let mut u = Bits::new();
            u.push(4, 8)
                .push(u64::from(i), 64)
                .push(8, 8)
                .push(u64::from(i), 32);
            assert!(h.update(seq, u, &mut notes));
        }
        h
    }

    #[test]
    fn the_join_snapshot_share_sheds_the_largest_history_instead_of_dooming() {
        let mut e = engine();
        let s = seat(&mut e, 1, 1001);
        let (big, small) = (long_history(900), long_history(150));
        let p = e.peer_mut(s).unwrap();
        p.objects.insert((1, 7), big);
        p.objects.insert((1, 8), small);
        let before = p.history_entries();
        // the private profile's share fitted to 12 players: 4096 / 11
        let cap = 372;
        assert!(before > cap + 300, "{before}");
        // the reference's pass (compaction and the per-object guard) cannot get there
        assert!(e.trim_history(s, cap) > cap);
        let left = e.shed_history(s, cap);
        assert_eq!(left, cap);
        let p = e.peer(s).unwrap();
        assert_eq!(p.history_entries(), cap);
        // the largest went first: the smaller history kept its updates
        let small_left = p.objects.get(&(1, 8)).unwrap().history_entries();
        assert_eq!(small_left, long_history(150).history_entries());
        assert!(!p.doomed);
    }

    /// A policy that verified every identity on every endpoint (as the world's admission does
    /// for the host's loopback and for FIUP-bound players).
    struct Verified;

    impl super::super::Policy for Verified {
        fn has_authorize(&self) -> bool {
            true
        }
        fn has_authorize_hook(&self) -> bool {
            true
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
    }

    /// The host synced at 192.0.2.1:3074 and seen just now, and its game back from `from`
    /// before its handshake completes: whether the new seat synced.
    fn rebind(verified: bool, from: &str) -> bool {
        let mut e = engine();
        if verified {
            e.policy = Box::new(Verified);
        }
        let host = seat(&mut e, 1, 1001);
        e.now = 10_000;
        e.peer_mut(host).unwrap().last_seen = 10_000;
        e.now = 10_100;
        let serial = e.next_serial;
        e.next_serial += 1;
        let peer = e.new_peer(serial, from.parse().unwrap(), 2, 1001, 9, 9);
        e.peers.push(peer);
        e.complete_handshake(serial);
        e.peer(serial).is_some_and(|p| p.synced)
    }

    #[test]
    fn a_verified_game_back_on_a_new_port_takes_its_seat_over_at_once() {
        assert!(rebind(true, "192.0.2.1:4000"));
        // another address, or no verified identity: the quiet rule (3 s) still holds
        assert!(!rebind(true, "198.51.100.9:4000"));
        assert!(!rebind(false, "192.0.2.1:4000"));
    }

    #[test]
    fn a_refusal_note_names_the_variable_never_the_value_sent() {
        use super::super::{Event, key::ObjKey, message::ObjectChange};
        let mut e = engine();
        let s = seat(&mut e, 1, 1001);
        // RoadTripPlayerSynced State (var 0, 4 bits) = 13: past its range (8)
        let mut vars = Bits::new();
        vars.push(0, 3).push(13, 4);
        let obj = ObjectChange {
            is_create: true,
            router_id: 9,
            key: ObjKey::own(5),
            ty: 2,
            sequence: 1,
            vars,
        };
        assert!(e.on_object(s, obj).is_none());
        let events = e.take_events();
        let note = events
            .iter()
            .find_map(|ev| match ev {
                Event::Note { id, args } if *id == "roadtrip.player_refused" => Some(args.clone()),
                _ => None,
            })
            .expect("the refusal is noted");
        assert_eq!(note[3], "OutOfRange");
        assert_eq!(note[4], 0, "the variable id");
        assert!(!note.iter().any(|a| a == 13), "{note:?}");
        // the controller's peer.rejected still gets the value (contract §4.5, not a log)
        assert!(events.iter().any(|ev| matches!(
            ev,
            Event::ValidationRejected {
                value: Some(13),
                ..
            }
        )));
    }
}
