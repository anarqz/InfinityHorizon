// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The seams of the features later steps port (README PORT_STATUS): what free roam needs from
//! them until then. Each function is where its feature plugs in:
//!
//! - events, races, the options and phasing are in [`super::races`]; the tag collision (36), tag
//!   and team events, Road Trip events and bucket-list universes plug in here (step 4);
//! - car meet invitations (step 9: 26, 27) and Road Trip (step 10: 47, 51): read, not acted on;
//! - bucket-list co-op (step 11): the server-owned slot array is in every free-roam snapshot (the
//!   game needs it to exist) and stays empty; RequestSynchronize, cancel and challenge end
//!   (62-64) are read, not acted on;
//! - the watch (step 13): nothing to measure yet;
//! - voice ([`super::voice`]) reads the teams through `team_event_active`, `team_of` and
//!   `hearing_group`; the teams port calls `voice_groups_changed` when they change.

use std::sync::Arc;

use super::{
    Engine, M, Peer,
    bits::Bits,
    packets::{BUCKET_LIST_ROUTER, BUCKET_LIST_SLOTS_TYPE},
    payload,
    wire::{self, Handle, Msg, Obj},
};

/// CarPredictionFull: the car's universe (5 bits) sits at this bit.
const CAR_UNIVERSE_OFFSET: usize = 395;

impl Engine {
    /// Bucket-list co-op is free roam's: no car meet or Road Trip world serves it.
    pub(super) fn bucket_list_world(&self) -> bool {
        self.settings.car_meet_lobby.is_none() && self.kind != super::EngineKind::RoadTrip
    }

    /// The server-owned BucketListSlotArraySynced (router 12, type 1, id 0) of a joiner's
    /// snapshot: each used entry's three variables; with no pairing yet, an empty create.
    pub(super) fn append_bucket_list_snapshot(&self, snapshot: &mut Vec<Bits>) {
        if !self.bucket_list_world() {
            return;
        }
        snapshot.push(wire::write(&Msg::Object(Obj {
            create: true,
            router: BUCKET_LIST_ROUTER,
            handle: Handle::own(0),
            ty: BUCKET_LIST_SLOTS_TYPE,
            seq: self.bucket_slots_sequence,
            payload: payload::bucket_list_slots(&[]),
        })));
    }

    /// The messages a later feature consumes (car meet invitations, Road Trip, bucket-list
    /// requests): true when the message is that feature's (read, not acted on yet).
    pub(super) fn other_seam(&mut self, serial: u64, kind: u8, body: &Bits) -> bool {
        match kind {
            26 => {
                self.invite_request(serial, body);
                true
            }
            27 => {
                self.join_or_cancel(serial, body);
                true
            }
            47 if self.is_road_trip() => {
                self.road_trip_vote(serial, body);
                true
            }
            51 if self.is_road_trip() => {
                self.road_trip_configured(serial);
                true
            }
            62..=64 => self.bucket_list_world(),
            _ => false,
        }
    }

    /// A car prediction for one receiver: across an event boundary it carries its owner's private
    /// universe; on the same side (or in universe 0) it goes as the game wrote it.
    pub(super) fn car_for(&self, sender: u64, receiver: u64, handle: Handle, body: &Bits) -> M {
        let universe = self
            .bucket_list_pair_universe(sender)
            .unwrap_or_else(|| self.peer(sender).map_or(0, |p| p.universe));
        let body = if self.same_side(sender, receiver) || universe == 0 {
            body.clone()
        } else {
            super::join::with_bits(
                body,
                CAR_UNIVERSE_OFFSET,
                payload::PRIVATE_UNIVERSE_BITS,
                u64::from(universe),
            )
        };
        Arc::new(wire::write(&Msg::Car { handle, body }))
    }

    /// A packet's car prediction and car events to every other synced player.
    pub(super) fn forward_cars(
        &mut self,
        sender: u64,
        car: Option<(Handle, Bits)>,
        events: Vec<M>,
    ) {
        if car.is_none() && events.is_empty() {
            return;
        }
        if let Some(c) = &car
            && let Some(p) = self.peer_mut(sender)
        {
            p.last_car = Some(c.clone());
        }
        for s in self.synced_serials_except(Some(sender)) {
            let mut messages = Vec::with_capacity(events.len() + 1);
            if let Some((handle, body)) = &car {
                messages.push(self.car_for(sender, s, *handle, body));
            }
            // car events (contacts) stay on their side of the event boundary
            if self.same_side(sender, s) {
                messages.extend(events.iter().cloned());
            }
            if !messages.is_empty() {
                self.send(s, messages, false, 0);
            }
        }
    }

    /// A player's router-1 object was created: the universe and team it was assigned before its
    /// object existed (none outside events) reach it now.
    pub(super) fn player_object_created(&mut self, serial: u64) {
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        p.player_sequence = None;
        let universe = p.universe;
        if universe != 0 {
            self.set_universe(serial, universe);
        }
        self.team_object_created(serial);
    }

    /// What the later features clear when a peer leaves.
    pub(super) fn peer_left_seams(&mut self, peer: &Peer) {
        self.release_keep.remove(&peer.serial);
        self.race_peer_left(peer.index, peer.serial);
        self.leave_event_side(peer.index, false);
        self.car_meet_peer_left(peer.serial);
        self.road_trip_peer_left(peer.serial, peer.index);
    }

    // ---- step 4: Road Trip, tag events, teams, bucket-list pairs, the watch ----

    /// The watch's exclusion at an event boundary (step 4).
    pub(super) fn watch_event_start(&mut self) {}

    /// A bucket-list co-op player stays in universe 0 during an event (step 4).
    pub(super) fn bucket_list_busy(&self, _serial: u64) -> bool {
        false
    }

    /// A bucket-list pair's universe (18..23, step 4) never clashes with event phasing.
    pub(super) fn bucket_list_universe_held(&self, _universe: u8) -> bool {
        false
    }

    /// A bucket-list partner's car carries its pair's universe (step 4).
    pub(super) fn bucket_list_pair_universe(&self, _serial: u64) -> Option<u8> {
        None
    }
}
