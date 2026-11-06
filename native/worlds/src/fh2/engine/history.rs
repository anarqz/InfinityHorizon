// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! One relayed object's history, replayed to a late joiner: the create with the updates folded
//! into it while they fit (`base`), then every later update that still matters (`tail`), in order.
//! An update is only removed when a later one provably supersedes it (a single-variable update
//! followed by another of the same variable, or an empty update), so a variable written once (a
//! player's name) is never lost; past a hard guard the oldest updates are folded or, logged once,
//! discarded.
//!
//! Sequences: the game applies an object's updates in order and waits on a gap, so a snapshot is a
//! contiguous run ending at the object's current sequence: the create carries `last - tail.len()`
//! and the kept updates follow one by one up to `last` (all plus `injected`, as every live copy).

use super::{
    bits::Bits,
    datagram::{self},
    dict::DotMap,
    key::ObjKey,
    message::{Message, ObjectChange},
    payload,
};
use crate::fh2::codec::object::{PAYLOAD_MAX_BITS, seq_forward, seq_is_newer, seq_next};

/// The kept updates are compacted at this count, then each time they double (at most half the
/// guard further), or past these bits, likewise doubling.
pub const COMPACT_AT: usize = 32;
pub const COMPACT_BITS: usize = 64 * 1024;
/// The hard guard after compaction (updates, bits).
pub const MAX_KEPT_UPDATES: usize = 4096;
pub const MAX_KEPT_BITS: usize = 512 * 1024;
/// Updates fold into the catch-up create only up to this many bits, so the create (plus the
/// server variables a snapshot adds: console flags, PrivateUniverseId, TeamId, a private leader's
/// IsLeader) always fits one
/// datagram; later updates stay separate tail messages.
pub const FOLD_MAX_BITS: usize = datagram::MAX_DATAGRAM_PAYLOAD_BITS - 128;
/// Out-of-order updates held past a gap.
pub const MAX_PENDING: usize = 16;

#[derive(Debug, Clone)]
pub struct History {
    compact_count: usize,
    compact_bits: usize,
    tail_bits: usize,
    guard_noted: bool,
    pub ty: u8,
    pub router: u8,
    pub base: Bits,
    pub last: u8,
    pub injected: u8,
    /// The server-owned console flags (router 3) replayed to late joiners.
    pub server_variables: DotMap<u8, bool>,
    /// A player object's server-set PrivateUniverseId (null when 0).
    pub universe: Option<u8>,
    /// A player object's server-set TeamId (null: never written).
    pub team: Option<u8>,
    /// A private session's leader's player object: its IsLeader (leader.rs).
    pub leader: bool,
    pub tail: Vec<(u8, Bits)>,
    pub pending: Vec<(u8, Bits)>,
    pub discarded: usize,
}

impl History {
    pub fn new(ty: u8, seq: u8, payload: Bits, router: u8) -> History {
        History {
            compact_count: COMPACT_AT,
            compact_bits: COMPACT_BITS,
            tail_bits: 0,
            guard_noted: false,
            ty,
            router,
            base: payload,
            last: seq,
            injected: 0,
            server_variables: DotMap::new(),
            universe: None,
            team: None,
            leader: false,
            tail: Vec::new(),
            pending: Vec::new(),
            discarded: 0,
        }
    }

    /// The create's sequence in a snapshot (before `injected`).
    pub fn create_sequence(&self) -> u8 {
        let v = (i64::from(self.last) - self.tail.len() as i64).rem_euclid(255);
        v as u8
    }

    pub fn history_bits(&self) -> usize {
        self.base.len() + self.tail_bits + self.pending.iter().map(|p| p.1.len()).sum::<usize>()
    }

    /// The kept entries, each its own buffer: the create, the tail and the pending updates.
    pub fn history_entries(&self) -> usize {
        1 + self.tail.len() + self.pending.len()
    }

    /// An update at `next`: false when it is old or a repeat (not relayed). `notes` gets the
    /// discarded count when the guard discards for the first time.
    pub fn update(&mut self, next: u8, update: Bits, notes: &mut Vec<usize>) -> bool {
        if next == seq_next(self.last) {
            self.apply(next, update, notes);
            while let Some(at) = self.pending.iter().position(|p| p.0 == seq_next(self.last)) {
                let (s, b) = self.pending.remove(at);
                self.apply(s, b, notes);
            }
            return true;
        }
        if !seq_is_newer(next, self.last) || self.pending.iter().any(|p| p.0 == next) {
            return false;
        }
        if self.pending.len() >= MAX_PENDING {
            // the one farthest ahead goes (the first of equals, as a max-by keeps)
            let last = i64::from(self.last);
            let mut far = 0usize;
            let mut best = -1i64;
            for (i, p) in self.pending.iter().enumerate() {
                let ahead = (i64::from(p.0) + 255 - last) % 255;
                if ahead > best {
                    best = ahead;
                    far = i;
                }
            }
            self.pending.remove(far);
        }
        self.pending.push((next, update));
        true
    }

    fn apply(&mut self, next: u8, update: Bits, notes: &mut Vec<usize>) {
        self.last = next;
        if self.tail.is_empty() && self.base.len() + update.len() <= FOLD_MAX_BITS {
            self.base.append(&update);
            return;
        }
        self.tail_bits += update.len();
        self.tail.push((next, update));
        if self.tail.len() >= self.compact_count || self.tail_bits > self.compact_bits {
            self.compact(notes);
        }
    }

    fn compact(&mut self, notes: &mut Vec<usize>) {
        self.supersede();
        if self.tail.len() > MAX_KEPT_UPDATES || self.tail_bits > MAX_KEPT_BITS {
            self.guard(notes);
        }
        self.rearm();
    }

    /// The peer's entry guard: compacts now and, while the tail is over half the hard guard,
    /// trims it as the hard guard does.
    pub fn trim(&mut self, notes: &mut Vec<usize>) {
        self.supersede();
        if self.tail.len() > MAX_KEPT_UPDATES / 2 || self.tail_bits > MAX_KEPT_BITS / 2 {
            self.guard(notes);
        }
        self.rearm();
    }

    fn rearm(&mut self) {
        let n = self.tail.len();
        self.compact_count = COMPACT_AT.max((n * 2).min(n + MAX_KEPT_UPDATES / 2 + 1));
        self.compact_bits = COMPACT_BITS.max(self.tail_bits * 2);
    }

    fn supersede(&mut self) {
        let mut newer = std::collections::HashSet::new();
        let mut kept = Vec::with_capacity(self.tail.len());
        for item in self.tail.drain(..).rev() {
            if item.1.is_empty() {
                continue;
            }
            if let Some(v) = payload::single_variable(self.router, &item.1)
                && !newer.insert(v)
            {
                continue;
            }
            kept.push(item);
        }
        kept.reverse();
        self.tail = kept;
        self.tail_bits = self.tail.iter().map(|p| p.1.len()).sum();
    }

    fn guard(&mut self, notes: &mut Vec<usize>) {
        self.shed(MAX_KEPT_UPDATES / 2, MAX_KEPT_BITS / 2, notes);
    }

    /// The peer's entry cap, past what [`History::trim`] reaches: the oldest kept updates go
    /// until at most `keep` are left (folded into the create while it fits, else discarded and
    /// noted once, as the hard guard does). The entries removed.
    pub fn shed_to(&mut self, keep: usize, notes: &mut Vec<usize>) -> usize {
        let before = self.tail.len();
        self.shed(keep, MAX_KEPT_BITS / 2, notes);
        self.rearm();
        before - self.tail.len()
    }

    fn shed(&mut self, keep_updates: usize, keep_bits: usize, notes: &mut Vec<usize>) {
        let mut discarded = 0usize;
        let mut drained = 0usize;
        while self.tail.len() - drained > keep_updates || self.tail_bits > keep_bits {
            let oldest = std::mem::take(&mut self.tail[drained].1);
            drained += 1;
            self.tail_bits -= oldest.len();
            if self.base.len() + oldest.len() <= FOLD_MAX_BITS {
                self.base.append(&oldest);
            } else {
                discarded += 1;
            }
        }
        self.tail.drain(..drained);
        self.discarded += discarded;
        if discarded == 0 || self.guard_noted {
            return;
        }
        self.guard_noted = true;
        notes.push(discarded);
    }

    /// The messages a joiner gets for this object under `handle`: the create, then the kept
    /// updates in a contiguous run, then the pending ones at their own sequences.
    pub fn snapshot(&mut self, router: u8, handle: ObjKey, notes: &mut Vec<usize>) -> Vec<Bits> {
        if !self.tail.is_empty() {
            self.compact(notes);
        }
        let mut payload = self.base.clone();
        if router == 3 && payload.len() + self.server_variables.len() * 5 > PAYLOAD_MAX_BITS {
            // a console's history may repeat its variables thousands of times: its last values
            // are enough for a snapshot and leave room for the server-owned flags
            let mut latest = std::collections::BTreeMap::new();
            for (index, value) in payload::console_variables(&payload).unwrap_or_default() {
                latest.insert(index, value);
            }
            payload = Bits::new();
            for (index, value) in latest {
                payload
                    .push(u64::from(index), 4)
                    .push(value, if index >= 7 { 2 } else { 1 });
            }
        }
        for (index, value) in self.server_variables.iter() {
            payload.push(u64::from(*index), 4).boolean(*value);
        }
        if router == 1
            && let Some(u) = self.universe
        {
            payload
                .push(u64::from(payload::PRIVATE_UNIVERSE_VARIABLE), 8)
                .push(u64::from(u), payload::PRIVATE_UNIVERSE_BITS);
        }
        if router == 1
            && let Some(t) = self.team
        {
            payload
                .push(u64::from(payload::TEAM_VARIABLE), 8)
                .push(u64::from(t), payload::TEAM_BITS);
        }
        if router == 1 && self.leader {
            payload
                .push(super::leader::IS_LEADER_VARIABLE, 8)
                .push(1, 1);
        }
        let mut seq = seq_forward(self.create_sequence(), self.injected);
        let mut out = Vec::with_capacity(1 + self.tail.len() + self.pending.len());
        out.push(Message::encode(&Message::Object(ObjectChange {
            is_create: true,
            router_id: router,
            key: handle,
            ty: self.ty,
            sequence: seq,
            vars: payload,
        })));
        for (_, update) in &self.tail {
            seq = seq_next(seq);
            out.push(Message::encode(&Message::Object(ObjectChange {
                is_create: false,
                router_id: router,
                key: handle,
                ty: 0,
                sequence: seq,
                vars: update.clone(),
            })));
        }
        for (s, update) in &self.pending {
            out.push(Message::encode(&Message::Object(ObjectChange {
                is_create: false,
                router_id: router,
                key: handle,
                ty: 0,
                sequence: seq_forward(*s, self.injected),
                vars: update.clone(),
            })));
        }
        out
    }
}
