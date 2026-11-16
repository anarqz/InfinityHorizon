//! Synthetic FM5SAVE1 saves for tests (CR-S105): made-up bytes in the container shape
//! [`crate::fm5::validate`] accepts, generated in code from a seed. Nothing here is derived from a
//! game file or a player's save (AGENTS.md "Absolute rules"); checks against real FM5 saves read
//! them at run time from `FI_REAL_SAVES_DIR`.
//!
//! What a synthetic FM5 career has:
//! - the profile container [`fm5::USER`]: `PlayerDatabase` (the `ssmc` envelope around a buffer
//!   that starts with SQLite's public header magic, then sparse seeded noise: not a working
//!   database), `ProfileData` (`ssmc`, payload length, a property tree with one `Main` group of
//!   `i32` children: `Level`, `XP`, `Credits`, `TotalWinnings`, `NumRaces`, `Seed`; zeros after
//!   it), `TransactionLogFile` (`u32` 1, `u32` 0), `VersionFlags` (`u32` 1), and its `commit`;
//! - per car a `BaseLivery_<carId>_<ts>` (seeded `C_livery` and `header`) and a
//!   `GarageThumbnail_<carId>_<ts>` (the PNG signature, then seeded bytes), each with its `commit`.

use super::{Rng, player_database};
use crate::fm5::{self, Container, LIVERY_PREFIX, THUMBNAIL_PREFIX, USER};

/// The parameters of one synthetic FM5 career. Equal specs give byte-identical bundles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The profile container's revision label.
    pub label: u64,
    /// Seeds every made-up byte.
    pub seed: u64,
    /// `Main/Level`.
    pub level: i32,
    /// `Main/XP`.
    pub xp: i32,
    /// `Main/Credits`.
    pub credits: i32,
    /// `Main/TotalWinnings`.
    pub winnings: i32,
    /// `Main/NumRaces`.
    pub races: i32,
    /// Cars in the garage: one livery and one thumbnail container each.
    pub cars: u32,
    /// `bigThumb.png` length (window 1 KiB to 512 KiB).
    pub big_thumb_len: usize,
}

impl Spec {
    /// A new career right after its first counted race (RE note §4's one-race shape, made-up
    /// numbers in the same relation: winnings plus the one-time grant).
    pub fn one_race(seed: u64) -> Self {
        Self {
            label: 0x12,
            seed,
            level: 1,
            xp: 4_000,
            credits: 39_000,
            winnings: 4_000,
            races: 1,
            cars: 1,
            big_thumb_len: 4 << 10,
        }
    }

    /// A career further on: `cars` cars, many races.
    pub fn played(seed: u64, cars: u32) -> Self {
        Self {
            label: 0x40,
            seed,
            level: 12,
            xp: 250_000,
            credits: 480_000,
            winnings: 610_000,
            races: 90,
            cars,
            big_thumb_len: 4 << 10,
        }
    }

    /// The career's containers, unpacked, in bundle order.
    pub fn containers(&self) -> Vec<Container> {
        let mut rng = Rng(self.seed ^ 0xF35A_5E5A_0000_0005);
        let mut user = vec![
            ("data/PlayerDatabase", player_database(&mut rng)),
            ("data/ProfileData", self.profile_data(&mut rng)),
            (
                "data/TransactionLogFile",
                [1u32, 0].map(u32::to_le_bytes).concat(),
            ),
            ("data/VersionFlags", 1u32.to_le_bytes().to_vec()),
        ];
        user.insert(0, ("commit", fm5::commit_for(&user)));
        let mut out = vec![Container {
            name: USER.to_owned(),
            label: self.label,
            files: user,
        }];
        let mut siblings = Vec::new();
        for i in 0..self.cars {
            let car = 100 + (self.seed % 1_000) as u32 * 7 + i * 13;
            let stamp = format!("2026110{}{:06}", 1 + i % 8, 100_000 + i);
            let mut livery = vec![
                ("data/C_livery", rng.bytes(96)),
                ("data/header", rng.bytes(128)),
            ];
            livery.insert(0, ("commit", fm5::commit_for(&livery)));
            let png = |rng: &mut Rng, n: usize| {
                let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
                b.extend(rng.bytes(n - 8));
                b
            };
            let mut thumb = vec![
                ("data/bigThumb.png", png(&mut rng, self.big_thumb_len)),
                ("data/smallThumb.png", png(&mut rng, 1 << 10)),
            ];
            thumb.insert(0, ("commit", fm5::commit_for(&thumb)));
            siblings.push(Container {
                name: format!("{LIVERY_PREFIX}{car}_{stamp}"),
                label: 1 + u64::from(i),
                files: livery,
            });
            siblings.push(Container {
                name: format!("{THUMBNAIL_PREFIX}{car}_{stamp}"),
                label: 2 + u64::from(i),
                files: thumb,
            });
        }
        siblings.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(siblings);
        out
    }

    /// The FM5SAVE1 bundle ([`fm5::pack`]).
    pub fn bundle(&self) -> Vec<u8> {
        fm5::pack(&self.containers())
    }

    fn profile_data(&self, rng: &mut Rng) -> Vec<u8> {
        const SIZE: usize = 20_488;
        let mut tree = Vec::new();
        let property = |out: &mut Vec<u8>, name: &str, kind: u32| {
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b" \0\0\0\0");
            out.extend_from_slice(&kind.to_le_bytes());
        };
        let ints = [
            ("Level", self.level),
            ("XP", self.xp),
            ("Credits", self.credits),
            ("TotalWinnings", self.winnings),
            ("NumRaces", self.races),
            ("Seed", rng.next() as i32),
        ];
        property(&mut tree, "Main", 0x0f);
        tree.extend_from_slice(&(ints.len() as u32).to_le_bytes());
        for (name, value) in ints {
            property(&mut tree, name, 3);
            tree.extend_from_slice(&value.to_le_bytes());
        }
        let mut out = b"ssmc".to_vec();
        out.extend_from_slice(&((SIZE - 8) as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&tree);
        out.resize(SIZE, 0);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_validate_and_read_their_numbers() {
        for spec in [
            Spec::one_race(1),
            Spec::played(2, 3),
            Spec {
                cars: 0,
                ..Spec::one_race(3)
            },
        ] {
            let bytes = spec.bundle();
            let v = fm5::validate(&bytes).unwrap_or_else(|e| panic!("{spec:?}: {e}"));
            assert_eq!(v.summary.level, Some(i64::from(spec.level)));
            assert_eq!(v.summary.xp, Some(i64::from(spec.xp)));
            assert_eq!(v.summary.credits, Some(i64::from(spec.credits)));
            assert_eq!(v.summary.winnings, Some(i64::from(spec.winnings)));
            assert_eq!(v.summary.cars, Some(i64::from(spec.cars)));
            assert_eq!(v.containers.len(), 1 + 2 * spec.cars as usize);
            assert_eq!(
                fm5::pack(&v.containers),
                bytes,
                "unpack, pack: the same bytes"
            );
            assert_eq!(spec.bundle(), bytes, "deterministic");
        }
        assert_ne!(
            fm5::validate(&Spec::one_race(1).bundle()).unwrap().content,
            fm5::validate(&Spec::one_race(2).bundle()).unwrap().content
        );
    }
}
