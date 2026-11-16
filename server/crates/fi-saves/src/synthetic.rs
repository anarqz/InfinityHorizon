//! Synthetic FH2SAVE1 saves for tests: made-up bytes in the container shape [`bundle::validate`]
//! accepts, generated in code from a seed. Nothing here is derived from a game file.
//!
//! The repository never holds bytes, strings or values taken from official game files or real
//! player saves (AGENTS.md "Absolute rules"). Every test that needs a save builds one here; the
//! few checks against real saves are `#[ignore]` tests that read them at run time from
//! `FI_REAL_SAVES_DIR` (outside the repository) and never copy them.
//!
//! What a synthetic save has, per file of [`SAVE_FILES`] (the size windows of
//! [`bundle::FILE_SIZES`]):
//! - `commit`: Azimuth's commit format ([`COMMIT_HEADER`], `blobs N`, one
//!   `<sha256> <size> <name>` line per data file), so the launcher's strict parser accepts it;
//! - `PlayerDatabase`: the `ssmc` envelope (tag, `u32` inner length) around a buffer that starts
//!   with SQLite's public file-header magic, then sparse seeded noise over zeros (it is not a
//!   working database: nothing in the save line opens it);
//! - `ProfileData`: zeros, seeded noise, and the ints [`bundle::summary`] reads (`TotalWinnings` only
//!   when the spec sets it), written
//!   with the marker layout of [`bundle::profile_int`];
//! - `SaveState`: made-up XML of the requested length;
//! - `BinarySaveState`, `TransactionLogFile`, `VersionFlags`: seeded bytes.
//!
//! [`Codec::Legacy`] packs like the Beta hooks before method 2 did: the large XML file as XPRESS
//! Huffman (method 1, match-free blocks from [`xpress_literal_encode`], several 64 KiB blocks
//! for a starter), the transaction log stored (method 0), the rest LZ (method 2). It exercises
//! every decoder. [`Codec::Canonical`] is [`bundle::pack`], what the launcher produces.

pub mod fm5;

use crate::bundle::{self, MAGIC, SAVE_FILES};
use crate::model::Digest;
use std::sync::OnceLock;

/// First line of every Azimuth commit file (re-exported by the launcher's `saves` module).
pub const COMMIT_HEADER: &str = bundle::COMMIT_HEADER;

/// How a synthetic bundle is packed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// [`bundle::pack`]: method 2 where smaller, else stored. Deterministic.
    Canonical,
    /// XPRESS (method 1) for `SaveState`, stored `TransactionLogFile`, method 2 for the rest.
    Legacy,
}

/// The parameters of one synthetic save. Equal specs give byte-identical bundles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    /// Bundle label (the Azimuth revision folder number).
    pub label: u64,
    /// Seeds every made-up byte: two seeds, two different saves (different content digests).
    pub seed: u64,
    /// `Credits` in ProfileData.
    pub credits: i32,
    /// `XP` in ProfileData.
    pub xp: i32,
    /// `Level` in ProfileData.
    pub level: i32,
    /// `TotalWinnings` in ProfileData (lifetime race credits); `None` leaves it out, so the save
    /// reads no winnings and its bytes are those of a spec without the field.
    pub winnings: Option<i32>,
    /// `(NumSkillTokens, SkillPointsTotal, UnspentSkillPointsTotal)`: ProfileData's token count and
    /// SaveState's `SkillsState`; `None` leaves them out (the bytes of a spec without them).
    pub skills: Option<(i32, i32, i32)>,
    /// `BinarySaveState` length (window 4 B to 256 KiB).
    pub binary_state_len: usize,
    /// `SaveState` length (window 1 KiB to 256 KiB).
    pub save_state_len: usize,
    /// Packing.
    pub codec: Codec,
}

impl Spec {
    /// A fresh-career save: label 1, legacy codec, a two-block XPRESS `SaveState`.
    pub fn starter(seed: u64) -> Self {
        Self {
            label: 1,
            seed,
            credits: 25_000,
            xp: 1_000,
            level: 1,
            winnings: None,
            skills: None,
            binary_state_len: 2_048,
            save_state_len: 80 << 10,
            codec: Codec::Legacy,
        }
    }

    /// An online-profile revision at the size minimums (`BinarySaveState` 4 B, small XML).
    pub fn online(label: u64, seed: u64) -> Self {
        Self {
            label,
            seed,
            credits: 310_000,
            xp: 420_000,
            level: 14,
            winnings: None,
            skills: None,
            binary_state_len: 4,
            save_state_len: 1_800,
            codec: Codec::Canonical,
        }
    }

    /// An offline-career revision with larger state files.
    pub fn offline(label: u64, seed: u64) -> Self {
        Self {
            label,
            seed,
            credits: 1_250_000,
            xp: 2_000_000,
            level: 31,
            winnings: None,
            skills: None,
            binary_state_len: 6_000,
            save_state_len: 24 << 10,
            codec: Codec::Canonical,
        }
    }

    /// The unpacked files, `commit` first, in [`SAVE_FILES`] order (what `validate` returns).
    pub fn files(&self) -> Vec<(&'static str, Vec<u8>)> {
        let mut rng = Rng(self.seed ^ 0x5EED_F00D_CAFE_0001);
        let data: Vec<(&'static str, Vec<u8>)> = vec![
            (
                "data/BinarySaveState",
                rng.bytes(self.binary_state_len.max(4)),
            ),
            ("data/PlayerDatabase", player_database(&mut rng)),
            ("data/ProfileData", self.profile_data(&mut rng)),
            (
                "data/SaveState",
                save_state(&mut rng, self.save_state_len.max(1 << 10), self.skills),
            ),
            ("data/TransactionLogFile", rng.bytes(256)),
            ("data/VersionFlags", vec![2, 0, 0, 0]),
        ];
        let mut files = Vec::with_capacity(SAVE_FILES.len());
        files.push(("commit", commit_text(&data)));
        files.extend(data);
        files
    }

    /// The packed FH2SAVE1 bundle.
    pub fn bundle(&self) -> Vec<u8> {
        let files = self.files();
        match self.codec {
            Codec::Canonical => bundle::pack(self.label, &files),
            Codec::Legacy => pack_legacy(self.label, &files),
        }
    }

    fn profile_data(&self, rng: &mut Rng) -> Vec<u8> {
        let mut out = vec![0u8; 20_488];
        for at in (2_048..20_000).step_by(512) {
            let noise = rng.bytes(8);
            out[at..at + 8].copy_from_slice(&noise);
        }
        let mut at = 64;
        let winnings = self.winnings.map(|w| ("TotalWinnings", w));
        let tokens = self.skills.map(|(t, _, _)| ("NumSkillTokens", t));
        for (name, value) in [
            ("Credits", self.credits),
            ("XP", self.xp),
            ("Level", self.level),
        ]
        .into_iter()
        .chain(winnings)
        .chain(tokens)
        {
            let mut field = (name.len() as u32).to_le_bytes().to_vec();
            field.extend_from_slice(name.as_bytes());
            field.extend_from_slice(b" \0\0\0\0\x03\0\0\0");
            field.extend_from_slice(&value.to_le_bytes());
            out[at..at + field.len()].copy_from_slice(&field);
            at += 64;
        }
        out
    }
}

/// A synthetic fresh career (`Spec::starter(1)`), built once per process.
pub fn starter() -> &'static [u8] {
    static STARTER: OnceLock<Vec<u8>> = OnceLock::new();
    STARTER.get_or_init(|| Spec::starter(1).bundle())
}

/// `files` (data files, `data/` prefix optional) as an Azimuth commit file.
pub fn commit_text(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut text = format!("{COMMIT_HEADER}\nblobs {}\n", files.len());
    for (name, body) in files {
        let short = name.trim_start_matches("data/");
        text.push_str(&format!(
            "{} {} {short}\n",
            Digest::of(body).hex(),
            body.len()
        ));
    }
    text.into_bytes()
}

/// XPRESS Huffman (MS-XCA) without matches: per 64 KiB block of output, a 256-byte table of
/// 4-bit code lengths for the 512 symbols, then the block's bytes as a fixed-length canonical code
/// over the byte values present (5 bits for a 30-symbol XML text), written most significant bit
/// first into 16-bit little-endian words. A valid method-1 stream for any input; it is smaller
/// than the input only when fewer than 128 byte values occur.
pub fn xpress_literal_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 260);
    for block in data.chunks(65_536) {
        let mut present = [false; 256];
        for &b in block {
            present[b as usize] = true;
        }
        let distinct = present.iter().filter(|p| **p).count() as u32;
        let len = distinct.next_power_of_two().trailing_zeros().max(1);
        let mut table = [0u8; 256];
        let mut codes = [0u32; 256];
        let mut next = 0u32;
        for symbol in 0..256usize {
            if present[symbol] {
                table[symbol / 2] |= (len as u8) << (4 * (symbol % 2));
                codes[symbol] = next;
                next += 1;
            }
        }
        out.extend_from_slice(&table);
        let (mut acc, mut bits, mut words) = (0u64, 0u32, 0usize);
        for &b in block {
            acc = (acc << len) | u64::from(codes[b as usize]);
            bits += len;
            while bits >= 16 {
                bits -= 16;
                out.extend_from_slice(&((acc >> bits) as u16).to_le_bytes());
                acc &= (1 << bits) - 1;
                words += 1;
            }
        }
        if bits > 0 {
            out.extend_from_slice(&((acc << (16 - bits)) as u16).to_le_bytes());
            words += 1;
        }
        // The decoder starts with two words buffered and refills one word per 16 bits consumed,
        // so the next block's table starts after that many words: pad up to it.
        let total = block.len() * len as usize;
        let read = 2 + total.saturating_sub(16).div_ceil(16);
        out.resize(out.len() + 2 * read.saturating_sub(words), 0);
    }
    out
}

fn pack_legacy(label: u64, files: &[(&'static str, Vec<u8>)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&label.to_le_bytes());
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (name, data) in files {
        let (method, body) = match *name {
            "data/SaveState" if xpress_literal_encode(data).len() < data.len() => {
                (1u8, xpress_literal_encode(data))
            }
            "data/TransactionLogFile" => (0u8, data.clone()),
            _ => {
                let packed = bundle::lz_encode(data);
                if !packed.is_empty() && packed.len() < data.len() {
                    (2u8, packed)
                } else {
                    (0u8, data.clone())
                }
            }
        };
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.push(method);
        out.extend_from_slice(&body);
    }
    out
}

fn player_database(rng: &mut Rng) -> Vec<u8> {
    const SIZE: usize = 6_291_464;
    let mut out = vec![0u8; SIZE];
    out[..4].copy_from_slice(b"ssmc");
    out[4..8].copy_from_slice(&((SIZE - 8) as u32).to_le_bytes());
    out[8..24].copy_from_slice(b"SQLite format 3\0");
    // Sparse noise keeps it seed-specific and still compressible (the 384 KiB bundle cap).
    for at in (4_096..SIZE - 64).step_by(65_536) {
        let noise = rng.bytes(16);
        out[at..at + 16].copy_from_slice(&noise);
    }
    out
}

fn save_state(rng: &mut Rng, len: usize, skills: Option<(i32, i32, i32)>) -> Vec<u8> {
    let close = b"</SyntheticSave>\n";
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<SyntheticSave seed=\"{:016x}\">\n",
        rng.next()
    )
    .into_bytes();
    // The game's `SkillsState` object, in the shape its SaveState writes (`bundle::save_state_skills`).
    if let Some((_, points, unspent)) = skills {
        out.extend_from_slice(
            format!(
                "  <Object Id=\"skills\" Type=\"SkillsState\">\n    <Property Name=\"SkillPointsTotal\" Value=\"{points}\"/>\n    <Property Name=\"UnspentSkillPointsTotal\" Value=\"{unspent}\"/>\n  </Object>\n"
            )
            .as_bytes(),
        );
    }
    let mut i = 0u32;
    while out.len() + close.len() + 48 < len {
        out.extend_from_slice(
            format!("  <Item k=\"{i}\" v=\"{:016x}\"/>\n", rng.next()).as_bytes(),
        );
        i += 1;
    }
    out.resize(len - close.len(), b' ');
    out.extend_from_slice(close);
    out
}

/// SplitMix64: deterministic on every platform.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n + 8);
        while out.len() < n {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::{FILE_SIZES, MAX_BUNDLE};

    #[test]
    fn xpress_literal_streams_decode_across_blocks() {
        for len in [0usize, 1, 2, 3, 255, 65_535, 65_536, 65_537, 140_001] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            let encoded = xpress_literal_encode(&data);
            assert_eq!(
                bundle::xpress_huffman_decode(&encoded, len).as_deref(),
                Some(data.as_slice()),
                "{len} bytes"
            );
        }
    }

    #[test]
    fn every_preset_validates_within_the_real_windows() {
        for spec in [
            Spec::starter(1),
            Spec::starter(2),
            Spec::online(0x12, 7),
            Spec::offline(0x10c, 9),
        ] {
            let data = spec.bundle();
            assert!(data.len() < MAX_BUNDLE, "{spec:?}: {} bytes", data.len());
            let valid = bundle::validate(&data).unwrap_or_else(|e| panic!("{spec:?}: {e}"));
            assert_eq!(valid.label, spec.label);
            assert_eq!(valid.files, spec.files(), "{spec:?}");
            assert_eq!(valid.summary.credits, Some(i64::from(spec.credits)));
            assert_eq!(valid.summary.xp, Some(i64::from(spec.xp)));
            assert_eq!(valid.summary.level, Some(i64::from(spec.level)));
            for (name, body) in &valid.files {
                let (_, lo, hi) = FILE_SIZES.iter().find(|(n, _, _)| n == name).unwrap();
                assert!((*lo..=*hi).contains(&(body.len() as u32)), "{name}");
            }
            assert_eq!(bundle::verify(&data).map(|v| v.content), Ok(valid.content));
        }
        assert_ne!(
            bundle::validate(&Spec::starter(1).bundle())
                .unwrap()
                .content,
            bundle::validate(&Spec::starter(2).bundle())
                .unwrap()
                .content,
            "seeds give different saves"
        );
        assert_eq!(Spec::starter(1).bundle(), starter(), "deterministic");
    }
}
