// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The seams of the features later steps port (README PORT_STATUS): what free roam needs from
//! them until then. Each function is where its feature plugs in:
//!
//! - events, races, the options and phasing are in [`super::races`]; the tag collision (36), tag
//!   and team events and Road Trip events plug in here (step 4);
//! - car meet invitations (step 9: 26, 27) and Road Trip (step 10: 47, 51): read, not acted on;
//! - bucket-list co-op is in [`super::bucket`]; 62-64 reach it from here;
//! - the watch is in [`super::watch`];
//! - voice ([`super::voice`]) reads the teams through `team_event_active`, `team_of` and
//!   `hearing_group`; the teams port calls `voice_groups_changed` when they change.

use std::sync::Arc;

use super::{
    Engine, M, Peer,
    bits::Bits,
    payload,
    wire::{self, Handle, Msg},
};

/// CarPredictionFull: the car's universe (5 bits) sits at this bit.
pub(super) const CAR_UNIVERSE_OFFSET: usize = 395;

impl Engine {
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
            62..=64 if self.bucket_list_world() => {
                self.bucket_list_message(serial, kind, body);
                true
            }
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
        self.bucket_list_peer_left(peer);
    }
}
