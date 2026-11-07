// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! FM5's GameOptions as the server keeps them (OD-206, W16; lobby spec §3 and §13): every var's
//! wire width, the reader of an owner's write, and the merged table a newcomer gets.
//!
//! A payload is a stream of `[9-bit var index][the var's value]`, each write a delta (lobby spec
//! §13.2). Every one of the 291 vars has a known width (§13.7 Q3), so every write is read whole
//! and merged; a write that names an index past 290 or ends inside a value is refused whole. The
//! table holds only the vars ever written (the server's defaults, then the owner's changes): the
//! console builds its own GameOptions with FM5's defaults, so the rest never needs sending.

use super::nerd::{self, Reader, Writer};

/// GameOptions' vars (lobby spec §3: register `0x140bfdd10`, 9-bit indexes).
pub const VARS: usize = 291;
/// A `SyncedString<wchar_t,24,0>` (rows 18, 65, 112, 159): a 5-bit length, then 16 bits per char.
const STR: u16 = 0;
pub const STRING_LENGTH_BITS: usize = 5;
pub const STRING_CHAR_BITS: usize = 16;

/// Each var's width in bits (`STR`: a string), by index. Generated from
/// `deprecated/beta/notes/local-docs/re/fm5-gameoptions-widths.tsv` (parent repo, origin/main
/// c4651591; `gowidths.py` from `map/go_vars.txt`), its `bits` column: SyncedInt N bits (a
/// float is 32 raw bits), SyncedEnum its third argument, SyncedBool 1, SyncedArray count × bits
/// with no length, 206-209 one 32-bit float each (`Nerd::TeamOptions`). The fixed widths add up
/// to 9837 bits, the spec's figure; 0, 3, 227 and 228 (8, 32, 3, 32) were checked live.
#[rustfmt::skip]
const WIDTHS: [u16; VARS] = [
    8, 8, 3, 32, 8, 8, 32, 32, 4, 4, 8, 3,  // 0
    1, 3, 1, 3, 8, 8, STR, 32, 1, 8, 8, 32,  // 12
    32, 32, 8, 16, 16, 32, 32, 32, 32, 32, 32, 2,  // 24
    8, 2, 8, 2, 8, 2, 8, 2, 16, 2, 16, 2,  // 36
    16, 2, 8, 2, 8, 2, 8, 2, 8, 2, 8, 2,  // 48
    8, 1, 32, 800, 800, STR, 32, 1, 8, 8, 32, 32,  // 60
    32, 8, 16, 16, 32, 32, 32, 32, 32, 32, 2, 8,  // 72
    2, 8, 2, 8, 2, 8, 2, 16, 2, 16, 2, 16,  // 84
    2, 8, 2, 8, 2, 8, 2, 8, 2, 8, 2, 8,  // 96
    1, 32, 800, 800, STR, 32, 1, 8, 8, 32, 32, 32,  // 108
    8, 16, 16, 32, 32, 32, 32, 32, 32, 2, 8, 2,  // 120
    8, 2, 8, 2, 8, 2, 16, 2, 16, 2, 16, 2,  // 132
    8, 2, 8, 2, 8, 2, 8, 2, 8, 2, 8, 1,  // 144
    32, 800, 800, STR, 32, 1, 8, 8, 32, 32, 32, 8,  // 156
    16, 16, 32, 32, 32, 32, 32, 32, 2, 8, 2, 8,  // 168
    2, 8, 2, 8, 2, 16, 2, 16, 2, 16, 2, 8,  // 180
    2, 8, 2, 8, 2, 8, 2, 8, 2, 8, 1, 32,  // 192
    800, 800, 32, 32, 32, 32, 2, 3, 1, 1, 1, 8,  // 204
    8, 32, 32, 32, 1, 8, 8, 8, 32, 32, 32, 3,  // 216
    32, 32, 8, 32, 32, 3, 1, 1, 1, 3, 3, 8,  // 228
    1, 1, 1, 1, 8, 96, 1, 8, 1, 32, 32, 8,  // 240
    8, 8, 8, 1, 1, 1, 1, 1, 2, 2, 2, 3,  // 252
    1, 1, 1, 1, 1, 1, 1, 1, 1, 3, 32, 16,  // 264
    16, 32, 32, 32, 32, 1, 1, 1, 1, 32, 4, 1,  // 276
    4, 1, 1,  // 288
];

/// How a var's value is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Bits(usize),
    String,
}

pub fn width(index: u32) -> Option<Width> {
    let w = *WIDTHS.get(usize::try_from(index).ok()?)?;
    Some(if w == STR {
        Width::String
    } else {
        Width::Bits(usize::from(w))
    })
}

/// One var of a write: its index and its value's bits as they came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Var {
    pub index: u32,
    pub value: Writer,
}

impl Var {
    pub fn new(index: u32, value: u64, bits: usize) -> Var {
        Var {
            index,
            value: Writer::new().push(value, bits),
        }
    }

    /// The value as a number when it fits 64 bits (not an array or a string).
    pub fn number(&self) -> Option<u64> {
        (self.value.len() <= 64).then(|| self.value.value())
    }
}

/// Why a write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// An index past the table (> 290).
    Unknown { index: u32 },
    /// The payload ends inside a var (`None`: inside its index).
    Truncated { index: Option<u32> },
}

/// Every var of a payload, in order; a payload that does not read whole is refused.
pub fn parse(payload: &Writer) -> Result<Vec<Var>, Refused> {
    let bytes = payload.bytes();
    let mut r = Reader::new(&bytes);
    r.length = payload.len();
    let mut vars = Vec::new();
    while r.position < r.length {
        let index = r
            .bits(nerd::GAME_OPTIONS_INDEX_BITS)
            .map_err(|_| Refused::Truncated { index: None })? as u32;
        let truncated = Refused::Truncated { index: Some(index) };
        let start = r.position;
        let bits = match width(index).ok_or(Refused::Unknown { index })? {
            Width::Bits(n) => n,
            Width::String => {
                let chars = r.bits(STRING_LENGTH_BITS).map_err(|_| truncated)? as usize;
                STRING_LENGTH_BITS + STRING_CHAR_BITS * chars
            }
        };
        if start + bits > r.length {
            return Err(truncated);
        }
        r.position = start + bits;
        vars.push(Var {
            index,
            value: Writer::from_bits(&bytes, start, bits),
        });
    }
    Ok(vars)
}

/// The payload of `vars`, in order.
pub fn encode(vars: &[Var]) -> Writer {
    let mut w = Writer::new();
    for v in vars {
        w.put(u64::from(v.index), nerd::GAME_OPTIONS_INDEX_BITS)
            .append(&v.value);
    }
    w
}

/// `vars` cut at var boundaries into payloads of at most `max_bits` each (a var longer than
/// that alone, never the case: the widest is 9 + 800 bits). No vars: no payload.
pub fn chunks(vars: &[Var], max_bits: usize) -> Vec<Writer> {
    let mut out = Vec::new();
    let mut current = Writer::new();
    for v in vars {
        let bits = nerd::GAME_OPTIONS_INDEX_BITS + v.value.len();
        if !current.is_empty() && current.len() + bits > max_bits {
            out.push(std::mem::take(&mut current));
        }
        current.append(&encode(std::slice::from_ref(v)));
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The server's GameOptions: the vars ever written, ascending by index, each at its last value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    table: Vec<Var>,
}

impl Options {
    /// The server's defaults, as (index, value, width).
    pub fn new(defaults: &[(u32, u64, usize)]) -> Options {
        let mut o = Options { table: Vec::new() };
        let vars: Vec<Var> = defaults.iter().map(|d| Var::new(d.0, d.1, d.2)).collect();
        o.merge(&vars);
        o
    }

    pub fn vars(&self) -> &[Var] {
        &self.table
    }

    /// A var's value, when it was written and fits 64 bits.
    pub fn value(&self, index: u32) -> Option<u64> {
        self.table
            .iter()
            .find(|v| v.index == index)
            .and_then(Var::number)
    }

    /// The whole table as one payload.
    pub fn full(&self) -> Writer {
        encode(&self.table)
    }

    pub fn merge(&mut self, vars: &[Var]) {
        for v in vars {
            match self.table.binary_search_by_key(&v.index, |t| t.index) {
                Ok(i) => self.table[i] = v.clone(),
                Err(i) => self.table.insert(i, v.clone()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_matches_the_spec() {
        let fixed: usize = WIDTHS.iter().map(|w| usize::from(*w)).sum();
        assert_eq!(fixed, 9837, "lobby spec §13.7: 9837 fixed bits");
        let strings: Vec<usize> = (0..VARS).filter(|i| WIDTHS[*i] == STR).collect();
        assert_eq!(strings, vec![18, 65, 112, 159]);
        for (index, bits) in [(0, 8), (3, 32), (210, 2), (227, 3), (228, 32), (245, 96)] {
            assert_eq!(width(index), Some(Width::Bits(bits)), "{index}");
        }
        for index in [63, 64, 110, 111, 157, 158, 204, 205] {
            assert_eq!(width(index), Some(Width::Bits(800)), "{index}");
        }
        for index in 206..=209 {
            assert_eq!(width(index), Some(Width::Bits(32)), "{index}");
        }
        assert_eq!(width(290), Some(Width::Bits(1)));
        assert_eq!(width(291), None);
    }

    #[test]
    fn a_string_reads_its_length_then_its_chars() {
        // var 18 = "AB", then var 3 = 7
        let w = Writer::new()
            .push(18, 9)
            .push(2, 5)
            .push(u64::from(b'A'), 16)
            .push(u64::from(b'B'), 16)
            .push(3, 9)
            .push(7, 32);
        let vars = parse(&w).unwrap();
        assert_eq!(vars.len(), 2);
        assert_eq!(vars[0].value.len(), 5 + 32);
        assert_eq!(vars[1].number(), Some(7));
        assert_eq!(encode(&vars), w);
        // cut inside the second char
        let short = Writer::from_bits(&w.bytes(), 0, 9 + 5 + 20);
        assert_eq!(parse(&short), Err(Refused::Truncated { index: Some(18) }));
    }

    #[test]
    fn chunks_cut_at_var_boundaries() {
        let vars: Vec<Var> = (63..=64)
            .map(|i| Var::new(i, 0, 800))
            .chain([Var::new(3, 9, 32)])
            .collect();
        let parts = chunks(&vars, 1000);
        assert_eq!(
            parts.iter().map(Writer::len).collect::<Vec<_>>(),
            vec![809, 809 + 41]
        );
        let mut again = Vec::new();
        for p in &parts {
            again.extend(parse(p).unwrap());
        }
        assert_eq!(again, vars);
    }
}
