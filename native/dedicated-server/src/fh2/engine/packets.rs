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
    history::History,
    payload,
    wire::{
        self, CAR_BODY_BITS, Handle, Header, Kind, Msg, NO_SEQUENCE, Obj, SAT_NAV_NODE_MAX,
        seq_add, seq_newer, seq_next,
    },
};

/// Most objects a peer keeps (its creates past it are relayed but not kept for late joiners).
const MAX_OBJECTS: usize = 256;
/// Updates kept for objects whose create has not come yet.
const MAX_ORPHANS: usize = 32;
/// A Road Trip player object's largest payload (router 9, type 2).
const ROAD_TRIP_PLAYER_MAX_BITS: usize = 256;
/// The bucket-list router and its player state type.
pub(super) const BUCKET_LIST_ROUTER: u8 = 12;
const BUCKET_LIST_PLAYER_TYPE: u8 = 0;
pub(super) const BUCKET_LIST_SLOTS_TYPE: u8 = 1;
/// What a snapshot adds to a player object (PrivateUniverseId 8+5, TeamId 8+1) and to a
/// console (nine server flags, 4+1 each).
const SERVER_PLAYER_BITS: usize = 13 + 9;
const SERVER_CONSOLE_BITS: usize = 9 * 5;
/// The time stamp's offset in a car prediction's body (its last 32 bits).
pub(super) const CAR_STAMP_OFFSET: usize = CAR_BODY_BITS - 32;

impl Engine {
    pub(super) fn receive_packet(&mut self, data: &[u8], endpoint: std::net::SocketAddr) -> Rr<()> {
        let Some(header) = Header::parse(data) else {
            return Ok(());
        };
        let body = &data[wire::HEADER_LEN..];
        let mut reader = Rd::new(body);
        let first = if header.count > 0 {
            let mut probe = reader;
            wire::read(&mut probe).ok()
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
                || restart.version != wire::VERSION
                || restart.game != wire::GAME
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
            .retain(|x| !wire::acknowledges(header.ack, header.ack_bits, x.id));
        if !admitted
            && header.id == 0
            && let Some(repeated) = &first_open
            && repeated.attempt == p.client_attempt
        {
            let own = p.attempt;
            if self.handshake_allowed(endpoint, xuid) {
                let m = Arc::new(wire::write(&Msg::OpenResponse {
                    flags: repeated.flags,
                    reason: 0,
                    attempt: repeated.attempt,
                    own_attempt: own,
                }));
                self.send(serial, vec![m], false, 0);
            }
            return Ok(());
        }
        let verdict = p.acks.accept(header.id);
        if verdict == wire::Verdict::Dropped {
            return Ok(()); // 0x3fff or more from the newest id: the game drops it silently
        }
        if verdict == wire::Verdict::Duplicate {
            if let Some(Msg::OpenResponse {
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
        let mut car_now: Option<(Handle, crate::fh2::engine::bits::Bits)> = None;
        let synced = p.synced;
        let (index, peer_attempt) = (p.index, p.attempt);
        let races = self.settings.races;
        let mut i = 0u32;
        while i < u32::from(header.count) && reader.left() >= 7 {
            i += 1;
            let message = match wire::read(&mut reader) {
                Ok(m) => m,
                Err(_) => {
                    self.rejected(xuid, "truncated_packet", None, None, None);
                    break;
                }
            };
            match message {
                Msg::Open {
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
                    answers.push(Arc::new(wire::write(&Msg::OpenResponse {
                        flags,
                        reason: 0,
                        attempt,
                        own_attempt: peer_attempt,
                    })));
                }
                Msg::OpenResponse {
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
                Msg::Close => {
                    self.drop_peer(serial, Why::Closed);
                    return Ok(());
                }
                Msg::Ping { time } => {
                    let wire_now = self.now_wire();
                    answers.push(Arc::new(wire::write(&Msg::Pong {
                        echo: time,
                        time: wire_now,
                    })));
                }
                Msg::Pong { echo, time } => {
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
                Msg::Finalise if !synced => early_finalise = true,
                _ if !synced => {}
                Msg::Object(obj) => {
                    if let Some(relayed) = self.on_object(serial, obj) {
                        if let Some(p) = self.peer_mut(serial) {
                            p.completion_confirmed = true;
                        }
                        forward.push(relayed);
                    }
                }
                Msg::Delete {
                    router,
                    handle,
                    seq,
                } if handle.kind == Kind::Own
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
                        .retain(|o| o.router != router || o.handle.id != handle.id);
                    let injected = removed.map_or(0, |o| o.injected);
                    forward.push(Arc::new(wire::write(&Msg::Delete {
                        router,
                        handle: Handle::relay(handle.id, index),
                        seq: seq_add(seq, injected),
                    })));
                }
                Msg::Car { handle, body } if handle.kind == Kind::Own => {
                    match payload::car_position(&body) {
                        Some(position) => {
                            let Some(p) = self.peer_mut(serial) else {
                                break;
                            };
                            p.completion_confirmed = true;
                            p.last_car_at = Some(now);
                            p.last_car_body = Some(body.clone());
                            p.car_position = Some(position);
                            p.car_id = Some(handle.id);
                            car_now = Some((Handle::relay(handle.id, index), body));
                        }
                        None => self.rejected(xuid, "invalid_car", Some(15), None, None),
                    }
                }
                Msg::CarEvent { kind, handle, body } if handle.kind == Kind::Own => {
                    if payload::car_event_ok(kind, &body) {
                        let change = Arc::new(wire::write(&Msg::CarEvent {
                            kind,
                            handle: Handle::relay(handle.id, index),
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
                Msg::GroupSatNav { node, .. } if (0..=SAT_NAV_NODE_MAX).contains(&node) => {
                    if let Some(p) = self.peer_mut(serial) {
                        p.completion_confirmed = true;
                    }
                    self.set_group_node(node);
                }
                Msg::Other { kind, body } if self.other_seam(serial, kind, &body) => {}
                Msg::Other { kind, body } if races => {
                    // a Ready ends the join's hold after this packet's own answers (an ownership
                    // refusal's sequence precedes the options replay), before any race step
                    let (answers_of, release) = self.race_message(serial, kind, &body);
                    reliable.extend(answers_of);
                    ready_release |= release;
                }
                Msg::TagCollision { .. } if races => {}
                Msg::Finalise => {
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
        if !answers.is_empty() || header.id != NO_SEQUENCE {
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
    fn on_object(&mut self, serial: u64, mut obj: Obj) -> Option<M> {
        let (xuid, index) = self.peer(serial).map(|p| (p.xuid, p.index))?;
        if self.settings.races && obj.router == 6 && obj.handle.kind == Kind::Transfer {
            // the event options journal: published by races.rs, never relayed as it came
            self.on_game_options(serial, &obj);
            return None;
        }
        let has = |e: &Engine, key: (u8, u32)| {
            e.peer(serial).is_some_and(|p| p.objects.contains_key(&key))
        };
        // the game makes its RoadTripPlayerSynced in every session type: relayed in every world
        let road_trip_player = obj.router == 9
            && obj.handle.kind == Kind::Own
            && obj.payload.len() <= ROAD_TRIP_PLAYER_MAX_BITS
            && if obj.create {
                obj.ty == 2
            } else {
                has(self, (9, obj.handle.id))
            };
        // a player's own BucketListPlayerStateSynced (router 12, type 0, id 0) in free roam
        let bucket_player = obj.router == BUCKET_LIST_ROUTER
            && self.bucket_list_world()
            && obj.handle.kind == Kind::Own
            && obj.handle.id == 0
            && if obj.create {
                obj.ty == BUCKET_LIST_PLAYER_TYPE
            } else {
                has(self, (BUCKET_LIST_ROUTER, 0))
            };
        if !road_trip_player {
            if !bucket_player
                && (obj.handle.kind != Kind::Own || !(1..=3).contains(&obj.router) || obj.ty != 0)
            {
                if obj.router == 9 {
                    self.note(
                        "roadtrip.object_refused",
                        vec![
                            json!(index),
                            json!(9),
                            json!(if obj.create { "True" } else { "False" }),
                            json!(obj.ty),
                            json!(format!("{:?}", obj.handle.kind)),
                            json!(obj.handle.id),
                            json!(obj.payload.len()),
                        ],
                    );
                }
                if obj.router == BUCKET_LIST_ROUTER {
                    self.bucket_list_refused(serial, &obj);
                }
                return None;
            }
            // a PlayerState the object store cannot keep is not relayed either
            if bucket_player
                && obj.create
                && !has(self, (BUCKET_LIST_ROUTER, 0))
                && self
                    .peer(serial)
                    .is_some_and(|p| p.objects.len() >= MAX_OBJECTS)
            {
                self.bucket_list_capped(serial);
                return None;
            }
            match payload::sanitize(obj.router, &obj.payload) {
                Ok(clean) => {
                    obj.payload = clean;
                    if obj.router == 1 {
                        self.observe_car_choice(serial, &obj.payload);
                    }
                }
                Err(fault) => {
                    let message = if obj.create { 7 } else { 8 };
                    self.rejected(
                        xuid,
                        "invalid_payload",
                        Some(message),
                        Some(u32::from(obj.router)),
                        Some(fault.value()),
                    );
                    // a bucket-list PlayerState fails open: its clean prefix, re-encoded
                    obj.payload = bucket_player
                        .then(|| self.bucket_list_sanitize_refused(serial, &obj, fault))
                        .flatten()?;
                }
            }
        }
        // ingest-size rule: a message whose relayed form (relay key, and the server variables a
        // snapshot adds) cannot fit one datagram is refused here, before it takes a sequence or
        // enters a history, never dropped later at send
        let server_bits = match obj.router {
            1 => SERVER_PLAYER_BITS,
            3 => SERVER_CONSOLE_BITS,
            _ => 0,
        };
        if obj.payload.len() + server_bits > wire::MAX_DATAGRAM_PAYLOAD_BITS {
            let message = if obj.create { 7 } else { 8 };
            let bits = obj.payload.len() as u32;
            self.rejected(
                xuid,
                "invalid_payload",
                Some(message),
                Some(u32::from(obj.router)),
                Some(bits),
            );
            self.note(
                "object.oversize_refused",
                vec![json!(index), json!(obj.router), json!(bits)],
            );
            return None;
        }
        let key = (obj.router, obj.handle.id);
        let mut guard_notes = Vec::new();
        let p = self.peer_mut(serial)?;
        if obj.create {
            if let Some(prev) = p.objects.get(&key)
                && !seq_newer(obj.seq, prev.last)
            {
                return None;
            }
            p.objects.remove(&key);
            if p.objects.len() < MAX_OBJECTS {
                let mut history = History::new(obj.ty, obj.seq, obj.payload.clone(), obj.router);
                let orphans: Vec<Obj> = p.orphans.drain(..).collect();
                for orphan in orphans {
                    if (orphan.router, orphan.handle.id) == key {
                        history.update(orphan.seq, orphan.payload, &mut guard_notes);
                    } else {
                        p.orphans.push_back(orphan);
                    }
                }
                p.objects.insert(key, history);
            }
            if obj.router == 1 {
                p.universe_due = true;
            }
            if obj.router == 3 && p.console_id != Some(obj.handle.id) {
                p.console_id = Some(obj.handle.id);
                p.console_sequence = None;
            }
            if obj.router == 3 {
                p.console_variable_sequences.clear();
                p.console_latest_sequence = Some(obj.seq);
            }
        } else if let Some(prev) = p.objects.get_mut(&key) {
            if !prev.update(obj.seq, obj.payload.clone(), &mut guard_notes) {
                for d in guard_notes {
                    self.guard_note(index, key, d);
                }
                return None;
            }
        } else {
            if p.orphans
                .iter()
                .any(|o| o.router == obj.router && o.handle.id == obj.handle.id && o.seq == obj.seq)
            {
                return None;
            }
            if p.orphans.len() >= MAX_ORPHANS {
                p.orphans.pop_front();
            }
            p.orphans.push_back(obj.clone());
        }
        if obj.router == 3 && p.console_id == Some(obj.handle.id) {
            if p.console_latest_sequence
                .is_none_or(|l| seq_newer(obj.seq, l))
            {
                p.console_latest_sequence = Some(obj.seq);
            }
            let mut observations = Vec::new();
            for (var, value) in payload::console_variables(&obj.payload).unwrap_or_default() {
                let newer = p
                    .console_variable_sequences
                    .get(&var)
                    .is_none_or(|last| seq_newer(obj.seq, *last));
                if newer {
                    p.console_variable_sequences.insert(var, obj.seq);
                    observations.push((var, value));
                }
            }
            let injected = p.objects.get(&key).map_or(0, |o| o.injected);
            for d in guard_notes.drain(..) {
                self.guard_note(index, key, d);
            }
            self.observe_console(serial, &observations);
            return Some(Arc::new(wire::write(&Msg::Object(Obj {
                handle: Handle::relay(obj.handle.id, index),
                seq: seq_add(obj.seq, injected),
                ..obj
            }))));
        }
        for d in guard_notes {
            self.guard_note(index, key, d);
        }
        if bucket_player {
            self.bucket_list_player_update(serial, obj.create, obj.seq, &obj.payload);
        }
        if obj.router == 1 {
            self.bucket_list_player_object(serial, &obj.payload); // the name 65 gives a partner
        }
        let injected = self
            .peer(serial)
            .and_then(|p| p.objects.get(&key))
            .map_or(0, |o| o.injected);
        Some(Arc::new(wire::write(&Msg::Object(Obj {
            handle: Handle::relay(obj.handle.id, index),
            seq: seq_add(obj.seq, injected),
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
        Some(Arc::new(wire::write(&Msg::Object(
            payload::console_update(id, seq, &[(1, true)]),
        ))))
    }

    fn set_group_node(&mut self, node: i32) {
        if node == self.group_node {
            return;
        }
        self.group_node = node;
        self.host_sequence = seq_next(self.host_sequence);
        let update = Arc::new(wire::write(&Msg::Object(Obj {
            create: false,
            router: 6,
            handle: Handle::own(0),
            ty: 0,
            seq: self.host_sequence,
            payload: payload::group_sat_nav(node),
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
        let update = Arc::new(wire::write(&Msg::Object(Obj {
            create: false,
            router: 10,
            handle: Handle::own(0),
            ty: 0,
            seq: self.weather_sequence,
            payload: self.weather.payload(),
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
                .map(|o| (o.serial, o.last_seen, o.index))
        };
        if let Some((old_serial, last_seen, old_index)) = old {
            if self.now - last_seen < self.settings.relay.takeover_quiet_ms as i64 {
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
            wire::write(&Msg::Object(Obj {
                create: true,
                router: 6,
                handle: Handle::own(0),
                ty: 0,
                seq: self.host_sequence,
                payload: host_payload,
            })),
            // the world's shared weather at its current sequence
            wire::write(&Msg::Object(Obj {
                create: true,
                router: 10,
                handle: Handle::own(0),
                ty: 0,
                seq: self.weather_sequence,
                payload: self.weather.payload(),
            })),
        ];
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
                snapshot.extend(obj.snapshot(key.0, Handle::relay(key.1, owner_index), &mut notes));
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
