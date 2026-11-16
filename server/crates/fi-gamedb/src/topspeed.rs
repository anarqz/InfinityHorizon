//! Each car's upgraded top-speed bound (OD-163, NR-5): the per-car speed limit of the peer
//! observations and the relay detector (MATCHMAKING_API.md §3.9 "The per-car speed limit").
//!
//! `fh2/topspeed.tsv` holds numbers only: `ordinal`, `max_kmh` (an upper bound for the car
//! fully upgraded, engine swaps included, gearing assumed free) and `confidence`. SAURON derived it
//! from the game's own database (`Data_Car.SimTopSpeed` as the stock speed, then the engine, boost
//! and drag upgrades as ratios to the game's stock power; 2026-10-31, private notes kept outside
//! this repository). All 354 `Data_Car` ordinals of the base catalogue are listed: the 336 player
//! cars, the 17 traffic cars and the placeholder 1215. The bound multiplies every gain, so it
//! over-estimates (safe for an anti-cheat); `low` rows (a swap with more than 4x the power, an
//! electric car, extreme drag or geometry) are the least sure.

use std::collections::BTreeMap;

/// The game data file of the table (`FI_GAME_DATA_DIR`; never in the crate).
pub const TOPSPEED_FILE: &str = "fh2/topspeed.tsv";

static FH2: fi_gamedata::Lazy<TopSpeeds> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(TOPSPEED_FILE, TopSpeeds::from_tsv));

/// How sure the bound is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confidence {
    /// The car's own engine, not gearing-limited stock.
    High,
    /// A gearing floor or a modest engine swap.
    Medium,
    /// A big swap, an electric motor, extreme drag or geometry: never trusted below the flat limit.
    Low,
}

/// One car's bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopSpeed {
    /// The upgraded top-speed bound, whole km/h.
    pub max_kmh: u32,
    /// How sure it is.
    pub confidence: Confidence,
}

/// The bounds by car ordinal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopSpeeds {
    by_ordinal: BTreeMap<i64, TopSpeed>,
}

impl TopSpeeds {
    /// The FH2 table of the game data ([`TOPSPEED_FILE`]), parsed once; panics with the game
    /// data's error ([`TopSpeeds::try_fh2`]).
    pub fn fh2() -> &'static TopSpeeds {
        &FH2
    }

    /// [`TopSpeeds::fh2`], or why the game data has none.
    pub fn try_fh2() -> Result<&'static TopSpeeds, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// Parses `ordinal<TAB>max_kmh<TAB>confidence` lines after the header; a repeated ordinal, a
    /// bound of 0 or an unknown confidence is an error.
    pub fn from_tsv(text: &str) -> Result<TopSpeeds, String> {
        let mut lines = text.lines();
        if lines.next().map(str::trim) != Some("ordinal\tmax_kmh\tconfidence") {
            return Err("header".into());
        }
        let mut by_ordinal = BTreeMap::new();
        for (n, line) in lines.enumerate().filter(|(_, l)| !l.trim().is_empty()) {
            let bad = || format!("line {}", n + 2);
            let mut cols = line.trim_end().split('\t');
            let (Some(ordinal), Some(max), Some(conf), None) =
                (cols.next(), cols.next(), cols.next(), cols.next())
            else {
                return Err(bad());
            };
            let ordinal: i64 = ordinal.parse().map_err(|_| bad())?;
            let max_kmh: u32 = max.parse().map_err(|_| bad())?;
            let confidence = match conf {
                "high" => Confidence::High,
                "medium" => Confidence::Medium,
                "low" => Confidence::Low,
                _ => return Err(bad()),
            };
            if max_kmh == 0 || ordinal < 0 {
                return Err(bad());
            }
            let row = TopSpeed {
                max_kmh,
                confidence,
            };
            if by_ordinal.insert(ordinal, row).is_some() {
                return Err(format!("{} repeats ordinal {ordinal}", bad()));
            }
        }
        Ok(TopSpeeds { by_ordinal })
    }

    /// The bound of car `ordinal`; `None` for a car the table does not list (a car-pack car).
    pub fn get(&self, ordinal: i64) -> Option<TopSpeed> {
        self.by_ordinal.get(&ordinal).copied()
    }

    /// Every row, by ordinal.
    pub fn iter(&self) -> impl Iterator<Item = (i64, TopSpeed)> + '_ {
        self.by_ordinal.iter().map(|(o, t)| (*o, *t))
    }

    /// Rows.
    pub fn len(&self) -> usize {
        self.by_ordinal.len()
    }

    /// No rows.
    pub fn is_empty(&self) -> bool {
        self.by_ordinal.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_table_lists_every_catalogue_car() {
        let t = TopSpeeds::fh2();
        assert_eq!(t.len(), 354);
        let row = |o| t.get(o).map(|r| (r.max_kmh, r.confidence));
        assert_eq!(row(247), Some((553, Confidence::Low)));
        assert_eq!(row(252), Some((491, Confidence::Medium)));
        assert_eq!(row(631), Some((200, Confidence::High)));
        assert_eq!(row(2176), Some((283, Confidence::High)));
        assert_eq!(row(2045), Some((591, Confidence::Medium)));
        assert_eq!(row(1558), Some((613, Confidence::Low)));
        assert_eq!(row(1674), Some((221, Confidence::Low)));
        assert_eq!(row(3000), None);
        let count = |c| t.iter().filter(|(_, r)| r.confidence == c).count();
        assert_eq!(
            (
                count(Confidence::High),
                count(Confidence::Medium),
                count(Confidence::Low)
            ),
            (71, 158, 125)
        );
    }

    #[test]
    fn a_malformed_table_is_refused() {
        let h = "ordinal\tmax_kmh\tconfidence\n";
        assert!(TopSpeeds::from_tsv(&format!("{h}1\t300\thigh\n")).is_ok());
        assert!(TopSpeeds::from_tsv("1\t300\thigh\n").is_err());
        assert!(TopSpeeds::from_tsv(&format!("{h}1\t300\thigh\n1\t310\tlow\n")).is_err());
        assert!(TopSpeeds::from_tsv(&format!("{h}1\t0\thigh\n")).is_err());
        assert!(TopSpeeds::from_tsv(&format!("{h}1\t300\tsure\n")).is_err());
        assert!(TopSpeeds::from_tsv(&format!("{h}1\t300\n")).is_err());
        assert!(TopSpeeds::from_tsv(&format!("{h}1\t300\thigh\tx\n")).is_err());
    }
}
