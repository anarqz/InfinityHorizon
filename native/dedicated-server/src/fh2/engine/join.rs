// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Quiet joins (contract §7.2, T25). The players already in a world reach a joiner in its
//! handshake snapshot; their cars (router 2), the event's options and the live car traffic wait for
//! the joiner's FinaliseSessionEntry. Until then the joiner is held: nothing of that kind goes to it.
//! At the finalise it gets the then-current snapshot of what was held, so nothing is lost or
//! reordered. A joiner that never finalises is released after 15 s. The cars' last predictions go
//! only once every reliable message of the release is acknowledged (or 10 s passed), so a parked
//! car's prediction never reaches a game before its object.

use std::{collections::HashSet, sync::Arc};

use serde_json::json;

use super::{
    Engine, M,
    packets::CAR_STAMP_OFFSET,
    wire::{self, Handle, Kind, Msg},
};

/// A joiner with no FinaliseSessionEntry this long after its sync is released anyway.
pub const QUIET_JOIN_BACKSTOP_MS: i64 = 15_000;
/// The cars' last predictions wait at most this long for the release's acknowledgements.
pub const CARS_PENDING_MS: i64 = 10_000;
const MAX_SKIPPED_NOTED: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Release {
    Finalise,
    Backstop,
    Ready,
}

/// Whether a message waits for its recipient's FinaliseSessionEntry.
fn held_for_joiner(m: &M) -> bool {
    matches!(
        wire::read_bits(m),
        Some(Msg::Object(wire::Obj {
            router: 2,
            handle: Handle {
                kind: Kind::Relay,
                ..
            },
            ..
        })) | Some(Msg::Delete {
            router: 2,
            handle: Handle {
                kind: Kind::Relay,
                ..
            },
            ..
        }) | Some(Msg::Car { .. })
            | Some(Msg::CarEvent { .. })
    )
}

fn ptr(m: &M) -> usize {
    Arc::as_ptr(m) as usize
}

impl Engine {
    /// What may go to the peer now: everything once released, else what is not held.
    pub(super) fn unheld(&self, serial: u64, messages: Vec<M>) -> Vec<M> {
        if !self.holding.contains_key(&serial) {
            return messages;
        }
        messages
            .into_iter()
            .filter(|m| !held_for_joiner(m))
            .collect()
    }

    /// The joiner's session entry is final (or its backstop ran out, or it readied for an event):
    /// what was held goes out, as it is now.
    pub(super) fn release_joiner(&mut self, serial: u64, why: Release) {
        let Some(since) = self.holding.remove(&serial) else {
            return;
        };
        if !self.alive(serial) {
            return;
        }
        let mut held: Vec<M> = Vec::new();
        for o in self.synced_serials_except(Some(serial)) {
            let mut guard = Vec::new();
            let Some(owner) = self.peer_mut(o) else {
                continue;
            };
            let owner_index = owner.index;
            for (key, obj) in owner.objects.iter_mut() {
                if key.0 != 2 {
                    continue;
                }
                let mut notes = Vec::new();
                held.extend(
                    obj.snapshot(key.0, Handle::relay(key.1, owner_index), &mut notes)
                        .into_iter()
                        .map(Arc::new),
                );
                guard.extend(notes.into_iter().map(|d| (*key, d)));
            }
            for (key, d) in guard {
                self.guard_note(owner_index, key, d);
            }
        }
        for m in &held {
            if !self.queue_reliable(serial, m.clone()) {
                break;
            }
        }
        self.pace(serial);
        let Some(p) = self.peer(serial) else { return };
        if p.doomed {
            return;
        }
        let index = p.index;
        let count = held.len();
        let release: HashSet<usize> = held.iter().map(ptr).collect();
        // the release's messages stay alive while they wait (their identity is the check)
        self.release_keep.insert(serial, held);
        self.cars_pending.insert(serial, (release, self.now));
        match why {
            Release::Backstop => self.note(
                "join.backstop",
                vec![json!(index), json!(QUIET_JOIN_BACKSTOP_MS), json!(count)],
            ),
            Release::Ready => self.note("join.ready", vec![json!(index), json!(count)]),
            Release::Finalise => {
                let waited = self.now - since;
                self.note(
                    "join.finalised",
                    vec![json!(index), json!(waited), json!(count)],
                )
            }
        }
        self.check_cars_pending();
    }

    /// Released joiners whose release was acknowledged (or waited 10 s) get the cars.
    pub(super) fn check_cars_pending(&mut self) {
        if self.cars_pending.is_empty() {
            return;
        }
        let pending: Vec<(u64, HashSet<usize>, i64)> = self
            .cars_pending
            .iter()
            .map(|(k, (r, s))| (*k, r.clone(), *s))
            .collect();
        for (serial, release, since) in pending {
            let Some(p) = self.peer(serial).filter(|p| !p.doomed) else {
                self.cars_pending.remove(&serial);
                self.release_keep.remove(&serial);
                continue;
            };
            let index = p.index;
            let unacknowledged = !release.is_empty()
                && (p.queue.iter().any(|m| release.contains(&ptr(m)))
                    || p.in_flight
                        .iter()
                        .any(|x| x.messages.iter().any(|m| release.contains(&ptr(m)))));
            if unacknowledged && self.now - since < CARS_PENDING_MS {
                continue;
            }
            self.cars_pending.remove(&serial);
            self.release_keep.remove(&serial);
            if unacknowledged {
                self.note(
                    "join.cars_unacknowledged",
                    vec![json!(index), json!(CARS_PENDING_MS)],
                );
            }
            self.send_last_cars(serial);
        }
    }

    /// Each other car's last prediction, one tick newer (the game ignores one that is not newer),
    /// kept as the car's last; a car whose object is gone is skipped (noted once).
    fn send_last_cars(&mut self, serial: u64) {
        let Some(to_index) = self.peer(serial).map(|p| p.index) else {
            return;
        };
        let senders: Vec<u64> = self
            .peers
            .iter()
            .filter(|p| p.serial != serial && p.synced && p.last_car.is_some())
            .map(|p| p.serial)
            .collect();
        for s in senders {
            let Some(sender) = self.peer(s) else { continue };
            let Some((handle, body)) = sender.last_car.clone() else {
                continue;
            };
            let sender_index = sender.index;
            if !sender.objects.contains_key(&(2, handle.id)) {
                if self.skipped_cars_noted.len() < MAX_SKIPPED_NOTED
                    && self.skipped_cars_noted.insert((sender_index, handle.id))
                {
                    self.note(
                        "join.car_missing",
                        vec![
                            json!(sender_index),
                            json!(handle.id),
                            json!(2),
                            json!(to_index),
                        ],
                    );
                }
                continue;
            }
            let body = restamp(&body);
            if let Some(sender) = self.peer_mut(s) {
                sender.last_car = Some((handle, body.clone()));
            }
            let car = self.car_for(s, serial, handle, &body);
            self.send(serial, vec![car], false, 0);
        }
    }

    pub(super) fn tick_quiet_joins(&mut self) {
        if !self.holding.is_empty() {
            let holding: Vec<(u64, i64)> = self.holding.iter().map(|(k, v)| (*k, *v)).collect();
            for (serial, since) in holding {
                if self.now - since >= QUIET_JOIN_BACKSTOP_MS {
                    self.release_joiner(serial, Release::Backstop);
                }
            }
        }
        self.check_cars_pending();
    }
}

/// The body with its time stamp one tick newer.
pub(super) fn restamp(body: &super::bits::Bits) -> super::bits::Bits {
    let mut r = body.reader();
    let stamp = r
        .skip(CAR_STAMP_OFFSET)
        .and_then(|_| r.read(32))
        .unwrap_or(0) as u32;
    with_bits(body, CAR_STAMP_OFFSET, 32, u64::from(stamp.wrapping_add(1)))
}

/// `body` with `width` bits at `offset` replaced by `value`.
pub(super) fn with_bits(
    body: &super::bits::Bits,
    offset: usize,
    width: u32,
    value: u64,
) -> super::bits::Bits {
    let mut r = body.reader();
    let mut out = super::bits::Bits::new();
    if let Ok(head) = r.take(offset) {
        out.append(&head);
    }
    let _ = r.skip(width as usize);
    out.push(value, width);
    let left = r.left();
    if let Ok(rest) = r.take(left) {
        out.append(&rest);
    }
    out
}
