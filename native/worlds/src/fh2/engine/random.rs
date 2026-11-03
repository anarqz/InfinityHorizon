// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Every random draw an engine makes (the D2 vectors' seams, vectors README "Seeds"):
//!
//! - [`EngineRandom`]: five independent SplitMix64 streams, one per purpose (weather, tag and
//!   team seeds, OpenConnection attempt ids, STUN transaction ids). A world seeds them from its
//!   own random source; a golden vector seeds them from its header.
//! - [`LegacyRandom`]: the seeded generator the weather, tag, team and Road Trip draws use, the
//!   subtractive generator (Knuth's ran3, as the .NET runtime's seeded `System.Random(int)` keeps
//!   it), so a world's weather reproduces exactly from its seed.

/// One SplitMix64 stream.
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> SplitMix64 {
        SplitMix64(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The purposes a seed is drawn for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedPurpose {
    Weather,
    Tag,
    Team,
}

/// The five streams' starting states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seeds {
    pub weather: u64,
    pub tag: u64,
    pub team: u64,
    pub attempt: u64,
    pub transaction: u64,
}

/// An engine's random source.
#[derive(Debug, Clone)]
pub struct EngineRandom {
    weather: SplitMix64,
    tag: SplitMix64,
    team: SplitMix64,
    attempt: SplitMix64,
    transaction: SplitMix64,
}

impl EngineRandom {
    pub fn new(s: Seeds) -> EngineRandom {
        EngineRandom {
            weather: SplitMix64::new(s.weather),
            tag: SplitMix64::new(s.tag),
            team: SplitMix64::new(s.team),
            attempt: SplitMix64::new(s.attempt),
            transaction: SplitMix64::new(s.transaction),
        }
    }

    /// Five streams drawn from one seed source (a world's own RNG).
    pub fn from_source(mut next: impl FnMut() -> u64) -> EngineRandom {
        EngineRandom::new(Seeds {
            weather: next(),
            tag: next(),
            team: next(),
            attempt: next(),
            transaction: next(),
        })
    }

    /// A seed for a [`LegacyRandom`], in `0..i32::MAX`.
    pub fn seed(&mut self, purpose: SeedPurpose) -> i32 {
        let s = match purpose {
            SeedPurpose::Weather => &mut self.weather,
            SeedPurpose::Tag => &mut self.tag,
            SeedPurpose::Team => &mut self.team,
        };
        (s.next_u64() % (i32::MAX as u64)) as i32
    }

    /// The engine's own OpenConnection attempt id of a new peer.
    pub fn attempt(&mut self) -> u32 {
        (self.attempt.next_u64() >> 32) as u32
    }

    /// Fills a STUN transaction id (or its tail): one draw per 8 bytes, little-endian, the last
    /// draw's unused bytes dropped.
    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let v = self.transaction.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    pub fn transaction(&mut self) -> [u8; 12] {
        let mut t = [0u8; 12];
        self.fill(&mut t);
        t
    }
}

/// The seeded subtractive generator (55 slots, lags 21 and 55, modulus `i32::MAX`).
#[derive(Debug, Clone)]
pub struct LegacyRandom {
    slots: [i32; 56],
    next: usize,
    nextp: usize,
}

const MBIG: i32 = i32::MAX;
const MSEED: i32 = 161_803_398;

impl LegacyRandom {
    pub fn new(seed: i32) -> LegacyRandom {
        let mut slots = [0i32; 56];
        let subtraction = if seed == i32::MIN {
            i32::MAX
        } else {
            seed.abs()
        };
        let mut mj = MSEED - subtraction;
        slots[55] = mj;
        let mut mk = 1i32;
        for i in 1..55 {
            let ii = (21 * i) % 55;
            slots[ii] = mk;
            mk = mj - mk;
            if mk < 0 {
                mk += MBIG;
            }
            mj = slots[ii];
        }
        for _ in 1..5 {
            for i in 1..56 {
                let n = slots[i].wrapping_sub(slots[1 + (i + 30) % 55]);
                slots[i] = if n < 0 { n + MBIG } else { n };
            }
        }
        LegacyRandom {
            slots,
            next: 0,
            nextp: 21,
        }
    }

    fn internal_sample(&mut self) -> i32 {
        let mut a = self.next + 1;
        if a >= 56 {
            a = 1;
        }
        let mut b = self.nextp + 1;
        if b >= 56 {
            b = 1;
        }
        let mut v = self.slots[a].wrapping_sub(self.slots[b]);
        if v == MBIG {
            v -= 1;
        }
        if v < 0 {
            v += MBIG;
        }
        self.slots[a] = v;
        self.next = a;
        self.nextp = b;
        v
    }

    fn sample(&mut self) -> f64 {
        f64::from(self.internal_sample()) * (1.0 / f64::from(MBIG))
    }

    /// `[0, 1)`.
    pub fn next_double(&mut self) -> f64 {
        self.sample()
    }

    /// `[0, max)` for `max` ≥ 0.
    pub fn next_below(&mut self, max: i32) -> i32 {
        (self.sample() * f64::from(max)) as i32
    }

    /// `[0, i32::MAX)`.
    pub fn next_int(&mut self) -> i32 {
        self.internal_sample()
    }
}
