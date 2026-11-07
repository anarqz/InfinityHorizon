// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The coalesced timer wheel (contract §8 "one loop thread ... a timer wheel"): deadlines are
//! rounded up to 10 ms slots so timers that fall close together fire in one loop turn, and each
//! timer key is scheduled at most once (`schedule` keeps an earlier deadline, `reschedule`
//! replaces it). Empty when nothing is due: the loop then sleeps until a datagram or a command
//! arrives (no timer with no worlds, no tick for an idle engine).

use std::collections::{BTreeMap, BTreeSet, HashMap};

pub const SLOT_MS: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Timer {
    /// Once a second while any world exists: leases, state refresh, memory, stats.
    Housekeeping,
    /// A world's coalesced `world.state`.
    StateFlush(String),
    /// A world's engine ticks (20 ms with synced peers, 100 ms with only handshaking ones).
    EngineTick(String),
    /// The drain's deadline.
    Drain,
}

#[derive(Debug, Default)]
pub struct Timers {
    slots: BTreeMap<u64, BTreeSet<Timer>>,
    at: HashMap<Timer, u64>,
}

fn slot(deadline: u64) -> u64 {
    deadline.div_ceil(SLOT_MS) * SLOT_MS
}

impl Timers {
    /// Schedules `t` at `deadline` unless it is already due earlier.
    pub fn schedule(&mut self, t: Timer, deadline: u64) {
        let s = slot(deadline);
        if let Some(&old) = self.at.get(&t) {
            if old <= s {
                return;
            }
            self.remove_at(&t, old);
        }
        self.slots.entry(s).or_default().insert(t.clone());
        self.at.insert(t, s);
    }

    /// Schedules `t` at `deadline`, replacing any earlier or later deadline.
    pub fn reschedule(&mut self, t: Timer, deadline: u64) {
        self.cancel(&t);
        self.schedule(t, deadline);
    }

    pub fn cancel(&mut self, t: &Timer) {
        if let Some(old) = self.at.remove(t) {
            self.remove_at(t, old);
        }
    }

    fn remove_at(&mut self, t: &Timer, s: u64) {
        if let Some(set) = self.slots.get_mut(&s) {
            set.remove(t);
            if set.is_empty() {
                self.slots.remove(&s);
            }
        }
    }

    pub fn is_scheduled(&self, t: &Timer) -> bool {
        self.at.contains_key(t)
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.slots.keys().next().copied()
    }

    /// Removes and returns every timer due at `now`, earliest slot first.
    pub fn pop_due(&mut self, now: u64) -> Vec<Timer> {
        let mut out = Vec::new();
        while let Some(&s) = self.slots.keys().next() {
            if s > now {
                break;
            }
            if let Some(set) = self.slots.remove(&s) {
                for t in set {
                    self.at.remove(&t);
                    out.push(t);
                }
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.at.len()
    }

    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }
}
