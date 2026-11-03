// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The event options (GameOptionsSynced, router 6, a transfer key): the bounded journal of the
//! owner's original writes, replayed one by one to a new owner and to joiners, and the reader of
//! its variables (a stream of `[10-bit id][value]`, ids ascending, each write a delta).
//!
//! The journal publishes a fragment only once every earlier source sequence has come (a gap holds
//! later fragments, and a gap open for `GameOptionsGapMs` ends the writer's session). It outlives
//! events (the owner's game writes only deltas), so [`Journal::compact`] rewrites it, when every
//! record reads, as the writes of its merged values.

use std::collections::{BTreeMap, HashMap};

use super::{
    bits::{Bits, Rd},
    wire::{MAX_PAYLOAD_BITS, seq_newer, seq_next},
};

/// The journal's bits (records and pending fragments together).
pub const MAX_BITS: usize = 64 * 1024;
/// A replay spans less than the modulo-255 sequence half-window, its grant included.
pub const MAX_RECORDS: usize = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Ignored,
    Empty,
    Stored,
    Buffered,
    Overflow,
}

/// The bounded journal of original GameOptions writes.
#[derive(Debug, Default)]
pub struct Journal {
    records: Vec<Bits>,
    pending: HashMap<u8, (Bits, i64)>,
    pub object_id: Option<u32>,
    /// The writer (a peer serial).
    pub source_owner: Option<u64>,
    source_sequence: Option<u8>,
    bits: usize,
    pending_bits: usize,
    gap_since: Option<i64>,
}

impl Journal {
    pub fn records(&self) -> &[Bits] {
        &self.records
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    fn change_owner(&mut self, owner: u64) {
        if self.source_owner == Some(owner) {
            return;
        }
        self.source_owner = Some(owner);
        self.source_sequence = None;
        // pending fragments were never published; the next writer inherits the accepted prefix
        self.clear_pending();
    }

    /// A valid ownership grant may move the writer's outgoing counter even for the same owner:
    /// its first sequence after the grant is not assumed.
    pub fn begin_source_epoch(&mut self, owner: u64) {
        self.source_owner = Some(owner);
        self.source_sequence = None;
        self.clear_pending();
    }

    /// One write from the owner; `ready` gets the fragments published by it, in order.
    pub fn apply(
        &mut self,
        owner: u64,
        id: u32,
        sequence: u8,
        payload: &Bits,
        now: i64,
        ready: &mut Vec<Bits>,
    ) -> Applied {
        ready.clear();
        if sequence == u8::MAX {
            return Applied::Ignored;
        }
        self.change_owner(owner);
        if self.object_id != Some(id) {
            self.object_id = Some(id);
            self.source_sequence = None;
            self.records.clear();
            self.bits = 0;
            self.clear_pending();
        }
        if self.pending.contains_key(&sequence)
            || self
                .source_sequence
                .is_some_and(|last| !seq_newer(sequence, last))
        {
            return Applied::Ignored;
        }
        let future = self
            .source_sequence
            .is_some_and(|previous| sequence != seq_next(previous));
        if payload.len() > MAX_PAYLOAD_BITS
            || payload.len() > MAX_BITS.saturating_sub(self.bits + self.pending_bits)
            || ((future || !payload.is_empty())
                && self.records.len() + self.pending.len() >= MAX_RECORDS)
        {
            return Applied::Overflow;
        }
        if future {
            self.pending_bits += payload.len();
            self.pending.insert(sequence, (payload.clone(), now));
            self.gap_since.get_or_insert(now);
            return Applied::Buffered;
        }
        self.commit(sequence, payload.clone(), ready);
        while let Some(last) = self.source_sequence
            && let Some((next, _)) = self.pending.remove(&seq_next(last))
        {
            self.pending_bits -= next.len();
            self.commit(seq_next(last), next, ready);
        }
        self.gap_since = self.pending.values().map(|p| p.1).min();
        if ready.is_empty() {
            Applied::Empty
        } else {
            Applied::Stored
        }
    }

    fn commit(&mut self, sequence: u8, fragment: Bits, ready: &mut Vec<Bits>) {
        // empty records advance the order but never enter the replay or the wire
        self.source_sequence = Some(sequence);
        if fragment.is_empty() {
            return;
        }
        self.bits += fragment.len();
        self.records.push(fragment.clone());
        ready.push(fragment);
    }

    /// Rewrites the records as the game's own writes of their merged values (the last write of
    /// each variable wins): ids ascending, each record no longer than the longest the game wrote,
    /// so replaying them sets what replaying the old ones did. False, changing nothing, when a
    /// record does not read.
    pub fn compact(&mut self) -> bool {
        if self.records.is_empty() {
            return true;
        }
        let Some(fields) = Fields::read(&self.records) else {
            return false;
        };
        let longest = self.records.iter().map(Bits::len).max().unwrap_or(0);
        let mut compacted = Vec::new();
        let mut record = Bits::new();
        for (id, raw) in &fields.values {
            if !record.is_empty() && record.len() + ID_BITS as usize + raw.len() > longest {
                compacted.push(std::mem::take(&mut record));
            }
            record.push(u64::from(*id), ID_BITS).append(raw);
        }
        if !record.is_empty() {
            compacted.push(record);
        }
        self.bits = compacted.iter().map(Bits::len).sum();
        self.records = compacted;
        true
    }

    /// The writer whose gap has been open for `timeout` ms.
    pub fn expired_source(&self, now: i64, timeout: i64) -> Option<u64> {
        match self.gap_since {
            Some(since) if now >= since && now - since >= timeout => self.source_owner,
            _ => None,
        }
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        self.pending_bits = 0;
        self.gap_since = None;
    }

    pub fn clear(&mut self) {
        self.object_id = None;
        self.source_owner = None;
        self.source_sequence = None;
        self.bits = 0;
        self.records.clear();
        self.clear_pending();
    }
}

/// A variable id's width.
pub const ID_BITS: u32 = 10;
pub const CAR_CLASS_ID: u16 = 30;
pub const MINIMUM_PI_ID: u16 = 31;
pub const MAXIMUM_PI_ID: u16 = 32;
pub const END_CONDITION_ID: u16 = 418;
pub const NUM_LAPS_ID: u16 = 419;
pub const NUM_TEAMS_ID: u16 = 428;
/// Var 0 GameTypePreset (7 King, 8 Infected).
pub const GAME_TYPE_PRESET_ID: u16 = 0;
pub const KING_PRESET: u64 = 7;
/// Var 420 TimedRaceLength (f32 seconds).
pub const TIMED_RACE_LENGTH_ID: u16 = 420;
/// TagOptions: TagItBucket 441, ItBucketBecomes 442, NotItBucketBecomes 443, NumPlayersPerIt 445.
pub const TAG_IT_BUCKET_ID: u16 = 441;
pub const IT_BUCKET_BECOMES_ID: u16 = 442;
pub const NOT_IT_BUCKET_BECOMES_ID: u16 = 443;
pub const NUM_PLAYERS_PER_IT_ID: u16 = 445;

/// The tag events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagMode {
    King,
    Infected,
}

/// A tag event's rules (TagOptions): the It bucket, what each car of a hit between an It car and
/// a not-It car becomes, and how many players share one It car at the start (King).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TagRules {
    pub mode: TagMode,
    pub it_bucket: i32,
    pub it_becomes: i32,
    pub not_it_becomes: i32,
    pub players_per_it: Option<i32>,
}

impl TagRules {
    pub const NOT_IT: i32 = 0;
    pub const IT: i32 = 1;
}
pub const INFECTED_PRESET: u64 = 8;
/// A bucket name: a 5-bit count of up to 24 UTF-16 units.
const NAME: i32 = -24;

/// One race bucket: its name, ids and flags, the class and PI limits, two 50 × u16 lists.
const BUCKET: [i32; 47] = [
    NAME, 32, 1, 8, 8, 32, 32, 32, 8, 16, 16, 32, 32, 32, 32, 32, 32, 2, 8, 2, 8, 2, 8, 2, 8, 2,
    16, 2, 16, 2, 16, 2, 8, 2, 8, 2, 8, 2, 8, 2, 8, 2, 8, 1, 32, 800, 800,
];
const HEAD: [i32; 22] = [
    8, 8, 3, 4, 32, 32, 8, 32, 32, 8, 32, 32, 4, 4, 8, 3, 2, 3, 1, 3, 8, 8,
];
/// After the eight buckets: the event, time, destination, scoring, tag (439-445), assists,
/// hopper, weather and the top-level flags (436 is three floats).
const TAIL: [i32; 95] = [
    32, 32, 2, 3, 1, 1, 1, 1, 8, 8, 32, 32, 32, 1, 8, 8, 8, 32, 32, 32, 3, 32, //
    32, 8, 32, 32, 4, 1, 1, 1, 2, 3, 8, 1, 1, 1, 1, 8, 96, 1, 1, //
    32, 32, 8, 8, 8, 8, 8, 1, 1, 1, 1, 1, 2, 2, 2, 3, 1, 1, 1, //
    1, 1, 1, 1, 1, 1, 3, 32, 16, 16, 32, 32, 32, 32, 1, 1, 1, //
    1, 32, 4, 1, 1, 32, 32, 32, 32, 1, 1, 4, 1, 1, 1, 1, 1, 1,
];
/// Ids 0..=492.
pub const VARIABLES: usize = HEAD.len() + 8 * BUCKET.len() + TAIL.len();

/// A variable's width in bits (negative: a string of at most that many units).
pub fn width(id: usize) -> Option<i32> {
    if id < HEAD.len() {
        return Some(HEAD[id]);
    }
    let id = id - HEAD.len();
    if id < 8 * BUCKET.len() {
        return Some(BUCKET[id % BUCKET.len()]);
    }
    TAIL.get(id - 8 * BUCKET.len()).copied()
}

/// The event's car restriction: a class (255 any) and a minimum and maximum PI (0 none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CarRule {
    pub class: i32,
    pub minimum_pi: i32,
    pub maximum_pi: i32,
}

impl CarRule {
    pub const ANY_CLASS: i32 = 255;
    pub const ANY: CarRule = CarRule {
        class: Self::ANY_CLASS,
        minimum_pi: 0,
        maximum_pi: 0,
    };

    /// Whether a car of `class` and performance index `pi` (×1000 shows its PI) may enter.
    pub fn accepts(&self, class: Option<i32>, pi: f32) -> bool {
        if *self == CarRule::ANY {
            return true;
        }
        let Some(have) = class else { return false };
        if !pi.is_finite() {
            return false;
        }
        let shown = (pi * 1000.0 + 0.5) as i32;
        (self.class == Self::ANY_CLASS || have == self.class)
            && (self.minimum_pi == 0 || shown >= self.minimum_pi)
            && (self.maximum_pi == 0 || shown <= self.maximum_pi)
    }
}

/// The options' variables read so far, each its raw bits (a string with its count).
#[derive(Debug, Default, Clone)]
pub struct Fields {
    pub values: BTreeMap<u16, Bits>,
}

impl Fields {
    /// A fresh reading of a whole journal; `None` when a record does not read.
    pub fn read(records: &[Bits]) -> Option<Fields> {
        let mut f = Fields::default();
        records.iter().all(|r| f.apply(r)).then_some(f)
    }

    /// Applies one write on top of the earlier ones; one that does not read changes nothing.
    pub fn apply(&mut self, write: &Bits) -> bool {
        let mut read = Vec::new();
        let mut r = write.reader();
        let mut previous: i64 = -1;
        while r.left() > 0 {
            if r.left() < ID_BITS as usize {
                return false;
            }
            let Ok(id) = r.read(ID_BITS) else {
                return false;
            };
            let id = id as usize;
            if (id as i64) <= previous || id >= VARIABLES {
                return false;
            }
            let Some(mut w) = width(id) else {
                return false;
            };
            if w < 0 {
                let mut probe: Rd<'_> = r;
                let Ok(count) = probe.read(5) else {
                    return false;
                };
                if count as i32 > -w {
                    return false;
                }
                w = 5 + 16 * count as i32;
            }
            if r.left() < w as usize {
                return false;
            }
            let Ok(raw) = r.take(w as usize) else {
                return false;
            };
            read.push((id as u16, raw));
            previous = id as i64;
        }
        for (id, raw) in read {
            self.values.insert(id, raw);
        }
        true
    }

    fn number(&self, id: u16) -> Option<u64> {
        let raw = self.values.get(&id)?;
        if raw.len() > 64 {
            return None;
        }
        raw.reader().read(raw.len() as u32).ok()
    }

    /// The primary race bucket's restriction (nothing written: any car).
    pub fn car_rule(&self) -> CarRule {
        CarRule {
            class: self
                .number(CAR_CLASS_ID)
                .map_or(CarRule::ANY_CLASS, |v| v as i32),
            minimum_pi: self.number(MINIMUM_PI_ID).map_or(0, |v| v as i32),
            maximum_pi: self.number(MAXIMUM_PI_ID).map_or(0, |v| v as i32),
        }
    }

    pub fn num_laps(&self) -> Option<u32> {
        self.number(NUM_LAPS_ID).map(|v| v as u32)
    }

    /// EndConditionType (0 none, 1 laps, 2 time, 3..6 other modes).
    pub fn end_condition(&self) -> Option<i32> {
        self.number(END_CONDITION_ID).map(|v| v as i32)
    }

    /// ScoringOptions.NumTeams (2 is "Teams: On").
    pub fn num_teams(&self) -> Option<i32> {
        self.number(NUM_TEAMS_ID).map(|v| v as i32)
    }

    /// TimedRaceLength (var 420, f32 seconds), when written as a finite value.
    pub fn timed_race_length(&self) -> Option<f32> {
        self.float(TIMED_RACE_LENGTH_ID)
    }

    fn float(&self, id: u16) -> Option<f32> {
        let bits = self.number(id)?;
        if self.values.get(&id)?.len() != 32 {
            return None;
        }
        let v = f32::from_bits(bits as u32);
        v.is_finite().then_some(v)
    }

    /// The tag rules of a King or Infected event (`None` otherwise); a variable not written keeps
    /// the game's default.
    pub fn tag(&self) -> Option<TagRules> {
        let mode = match self.number(GAME_TYPE_PRESET_ID)? {
            KING_PRESET => TagMode::King,
            INFECTED_PRESET => TagMode::Infected,
            _ => return None,
        };
        let byte = |id: u16, fallback: i32| self.number(id).map_or(fallback, |v| v as i32);
        Some(TagRules {
            mode,
            it_bucket: byte(TAG_IT_BUCKET_ID, TagRules::IT),
            it_becomes: byte(
                IT_BUCKET_BECOMES_ID,
                if mode == TagMode::Infected {
                    TagRules::IT
                } else {
                    TagRules::NOT_IT
                },
            ),
            not_it_becomes: byte(NOT_IT_BUCKET_BECOMES_ID, TagRules::IT),
            players_per_it: self.number(NUM_PLAYERS_PER_IT_ID).map(|v| v as i32),
        })
    }

    /// A King or Infected preset (a tag event).
    pub fn is_tag(&self) -> bool {
        matches!(
            self.number(GAME_TYPE_PRESET_ID),
            Some(KING_PRESET | INFECTED_PRESET)
        )
    }
}
