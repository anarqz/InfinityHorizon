// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The seams of the features later steps port (README PORT_STATUS): what free roam needs from
//! them until then. Each function is where its feature plugs in:
//!
//! - events and races (step 7): ownership of the event options (10), race requests (38), Ready
//!   (41), scores (43-45), the tag collision (36); nothing happens before the port;
//! - phasing (step 8): no event exists, so every player is on the same side in universe 0 and a
//!   car prediction goes out as the game wrote it;
//! - car meet invitations (step 9: 26, 27) and Road Trip (step 10: 47, 51): read, not acted on;
//! - bucket-list co-op (step 11): the server-owned slot array is in every free-roam snapshot (the
//!   game needs it to exist) and stays empty; RequestSynchronize, cancel and challenge end
//!   (62-64) are read, not acted on;
//! - voice (step 12) and the watch (step 13): nothing to measure or forward yet.

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

/// MsgReadyRequest (41): the lobby's event key, then the ready flag.
pub(super) fn ready_request(body: &Bits) -> Option<bool> {
    let mut r = body.reader();
    Handle::read(&mut r, 2).ok()?;
    r.boolean().ok()
}

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
    pub(super) fn other_seam(&mut self, _serial: u64, kind: u8, _body: &Bits) -> bool {
        match kind {
            26 | 27 => true,
            47 | 51 => self.kind == super::EngineKind::RoadTrip,
            62..=64 => self.bucket_list_world(),
            _ => false,
        }
    }

    /// An event message (`races` worlds): the reliable answers to the sender.
    pub(super) fn race_message_seam(&mut self, _serial: u64, _kind: u8, _body: &Bits) -> Vec<M> {
        Vec::new()
    }

    /// Whether two players are on the same side of an event boundary (always, until phasing).
    fn same_side(&self, _a: u64, _b: u64) -> bool {
        true
    }

    /// A car prediction for one receiver: across an event boundary it carries its owner's private
    /// universe; on the same side (or in universe 0) it goes as the game wrote it.
    pub(super) fn car_for(&self, sender: u64, receiver: u64, handle: Handle, body: &Bits) -> M {
        let universe = self.peer(sender).map_or(0, |p| p.universe);
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
            messages.extend(events.iter().cloned());
            if !messages.is_empty() {
                self.send(s, messages, false, 0);
            }
        }
    }

    /// A player's router-1 object was created: the universe and team it was assigned before its
    /// object existed (none outside events) reach it now.
    pub(super) fn player_object_created(&mut self, serial: u64) {
        if let Some(p) = self.peer_mut(serial) {
            p.player_sequence = None;
        }
    }

    /// What the later features clear when a peer leaves.
    pub(super) fn peer_left_seams(&mut self, peer: &Peer) {
        self.release_keep.remove(&peer.serial);
    }
}
