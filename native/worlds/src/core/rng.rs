// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The engine's deterministic random source (splitmix64). The runtime seeds it from the OS
//! (`seed_from_os`); a golden vector seeds it from its header, so a replay is exact. Not for keys:
//! nothing secret is drawn from it.

#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// 0..i32::MAX, as the C# `RandomNumberGenerator.GetInt32(int.MaxValue)` range.
    pub fn seed_i32(&mut self) -> i32 {
        (self.next_u64() % (i32::MAX as u64)) as i32
    }

    /// A random (version 4) UUID, canonical lowercase.
    pub fn uuid(&mut self) -> String {
        let (a, b) = (self.next_u64(), self.next_u64());
        let a = (a & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000;
        let b = (b & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000;
        format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            a >> 32,
            (a >> 16) & 0xFFFF,
            a & 0xFFFF,
            b >> 48,
            b & 0xFFFF_FFFF_FFFF
        )
    }
}

/// A seed from what std offers without a dependency: RandomState's per-process random keys and
/// the clock.
pub fn seed_from_os() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(t.as_nanos());
    }
    h.write_u32(std::process::id());
    h.finish()
}
