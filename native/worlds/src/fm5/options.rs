// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! FM5's GameOptions as the server keeps them (OD-206, W16; lobby spec §3 and §13): the merged
//! table every newcomer gets in its first full Update, and the owner's writes that could not be
//! merged, replayed after it in order.
//!
//! A payload is a stream of `[9-bit var index][the var's value]` (each write a delta). Only the
//! vars whose width the lobby spec reads (§3) are parsed; the set the owner writes is OPEN (track
//! 3, end type 227, laps 228, preset 0, 2, 4 and the restriction ranges are expected). A write that
//! reads completely is merged into the table; one with a var of unknown width (or one that does
//! not end where its size says) is kept as it is in the tail, and so is every write after it, so
//! a newcomer replays them in their original order and no merged value overtakes an older raw one
//! (FH2's journal, `fh2::engine::options`, compacts the same way once every record reads).

use super::nerd::{self, Reader, Writer};

/// The tail's writes, at most: the joiner's options (the full Update, the tail, a grant) go in one
/// packet, so the console applies them in order.
pub const TAIL_MAX_RECORDS: usize = 32;
/// The tail's bits, at most (one datagram with room for the table and the headers).
pub const TAIL_MAX_BITS: usize = 6000;

/// A GameOptions var's width in bits, for the vars the lobby spec §3 reads (none of the INFERRED
/// widths: 6 and 7 f32, 11-15, 206-209; nor the bucket and restriction vars 18-205).
pub fn width(index: u32) -> Option<usize> {
    Some(match index {
        // GameTypePreset, EnvironmentFilter
        0 | 1 => 8,
        // ETrackFilterType
        2 => 3,
        // TrackId
        3 => 32,
        // TimeOfDayIndex, CollObjLayoutId
        4 | 5 => 8,
        // MaxPlayers, MaxAIPlayers
        8 | 9 => 4,
        // NumPlayerBuckets, Series
        10 | 16 | 17 => 8,
        // EStartType, EGridOrderingType, EEndConditionType, EScoringType
        210 | 211 | 227 | 233 => 3,
        // NumLaps (seen live as "3 Laps" with this width)
        228 => 32,
        _ => return None,
    })
}

/// A payload read with [`width`]: the vars read, in order, and where the read stopped when it
/// did not reach the end (`stop`: the var index of unknown width, or `None` for a payload that
/// ends inside a value or a var whose index does not fit, and the bits left from that index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub vars: Vec<(u32, u64, usize)>,
    pub unread: Option<(Option<u32>, usize)>,
}

impl Parsed {
    pub fn complete(&self) -> bool {
        self.unread.is_none()
    }
}

pub fn parse(payload: &Writer) -> Parsed {
    let bytes = payload.bytes();
    let mut r = Reader::new(&bytes);
    r.length = payload.len();
    let mut vars = Vec::new();
    while r.position < r.length {
        let at = r.position;
        let left = r.length - at;
        let Ok(index) = r.bits(nerd::GAME_OPTIONS_INDEX_BITS) else {
            return Parsed {
                vars,
                unread: Some((None, left)),
            };
        };
        let index = index as u32;
        let Some(bits) = width(index) else {
            return Parsed {
                vars,
                unread: Some((Some(index), left)),
            };
        };
        let Ok(value) = r.bits(bits) else {
            return Parsed {
                vars,
                unread: Some((None, left)),
            };
        };
        vars.push((index, value, bits));
    }
    Parsed { vars, unread: None }
}

/// What [`Options::apply`] did with a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Merged into the table.
    Merged,
    /// Kept in the tail, replayed after the table.
    Tail,
    /// Neither (the tail is full): consoles already in the lobby have it, a newcomer will not.
    Lost,
}

/// The server's GameOptions: the merged table, then the tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    table: Vec<(u32, u64, usize)>,
    tail: Vec<Writer>,
    tail_bits: usize,
}

impl Options {
    pub fn new(table: Vec<(u32, u64, usize)>) -> Options {
        let mut o = Options {
            table: Vec::new(),
            tail: Vec::new(),
            tail_bits: 0,
        };
        o.merge(&table);
        o
    }

    /// The merged vars, ascending by index.
    pub fn table(&self) -> &[(u32, u64, usize)] {
        &self.table
    }

    pub fn tail(&self) -> &[Writer] {
        &self.tail
    }

    /// A merged var's value.
    pub fn value(&self, index: u32) -> Option<u64> {
        self.table.iter().find(|v| v.0 == index).map(|v| v.1)
    }

    /// The full Update's payload (the merged table).
    pub fn full(&self) -> Writer {
        nerd::vars_payload(&self.table, nerd::GAME_OPTIONS_INDEX_BITS)
    }

    fn merge(&mut self, vars: &[(u32, u64, usize)]) {
        for v in vars {
            match self.table.binary_search_by_key(&v.0, |t| t.0) {
                Ok(i) => self.table[i] = *v,
                Err(i) => self.table.insert(i, *v),
            }
        }
    }

    /// One write of the owner's, read as `parsed`.
    pub fn apply(&mut self, payload: &Writer, parsed: &Parsed) -> Applied {
        if self.tail.is_empty() && parsed.complete() {
            self.merge(&parsed.vars);
            return Applied::Merged;
        }
        if self.tail.len() >= TAIL_MAX_RECORDS || self.tail_bits + payload.len() > TAIL_MAX_BITS {
            return Applied::Lost;
        }
        self.tail_bits += payload.len();
        self.tail.push(payload.clone());
        Applied::Tail
    }
}
