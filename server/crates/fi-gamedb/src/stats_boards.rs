//! The game's stats boards (scoreboard type 4) and its speed boards (9/5, 9/6), traced from the
//! submit path (T-PD-S5, `fh2/stats-boards-v2.json`; docs/web/PLAY_STATS_BOARDS_V2_METHOD.md):
//! the score type of each ambient activity and, where the game has one, its largest value
//! (`MaxAchievable/*`, or the catalog's car models for the Horizon Promo, 4/7). It supersedes the
//! scoreboard-type-4 rows of `fh2/leaderboards.json`, which T-PD-S3 inferred from enum order
//! (wrong for 4/7 to 4/12). Every value is a snapshot (a running total or the best skill chain),
//! never an increment; the board keeps the best.

use serde::Deserialize;
use std::collections::BTreeMap;

/// The game data file of the table (`FI_GAME_DATA_DIR`; never in the crate).
pub const STATS_BOARDS_FILE: &str = "fh2/stats-boards-v2.json";

static FH2: fi_gamedata::Lazy<StatsBoards> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(STATS_BOARDS_FILE, StatsBoards::from_json));

/// One board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatBoard {
    /// 4 (stats) or 9 (speed).
    pub scoreboard_type: i32,
    /// The score type.
    pub score_type: i32,
    /// The game's `AmbientLeaderboardType` name ("BarnFinds").
    pub ambient_name: String,
    /// The largest value the game allows, `None` without one (head-to-head wins, skill chains,
    /// the speed boards).
    pub max: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBoard {
    scoreboard_type: i32,
    score_type: i32,
    ambient_name: String,
    #[serde(default)]
    max: Option<i64>,
}

#[derive(Deserialize)]
struct Raw {
    boards: Vec<RawBoard>,
}

/// The boards by `(scoreboardType, scoreType)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatsBoards {
    by_key: BTreeMap<(i32, i32), StatBoard>,
}

impl StatsBoards {
    /// The table of the game data ([`STATS_BOARDS_FILE`]), parsed once; panics with the game
    /// data's error ([`StatsBoards::try_fh2`]).
    pub fn fh2() -> &'static StatsBoards {
        &FH2
    }

    /// [`StatsBoards::fh2`], or why the game data has none.
    pub fn try_fh2() -> Result<&'static StatsBoards, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// Parses the table; a repeated key or a negative maximum is an error.
    pub fn from_json(text: &str) -> Result<StatsBoards, String> {
        let raw: Raw = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut by_key = BTreeMap::new();
        for b in raw.boards {
            if b.max.is_some_and(|m| m < 0) {
                return Err(format!("{}/{} max", b.scoreboard_type, b.score_type));
            }
            let key = (b.scoreboard_type, b.score_type);
            let board = StatBoard {
                scoreboard_type: b.scoreboard_type,
                score_type: b.score_type,
                ambient_name: b.ambient_name,
                max: b.max,
            };
            if by_key.insert(key, board).is_some() {
                return Err(format!("{}/{} repeated", key.0, key.1));
            }
        }
        Ok(StatsBoards { by_key })
    }

    /// The board `(scoreboard_type, score_type)`.
    pub fn board(&self, scoreboard_type: i32, score_type: i32) -> Option<&StatBoard> {
        self.by_key.get(&(scoreboard_type, score_type))
    }

    /// The stats boards (scoreboard type 4), by score type.
    pub fn stats(&self) -> impl Iterator<Item = &StatBoard> {
        self.by_key.values().filter(|b| b.scoreboard_type == 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_traced_mapping() {
        let t = StatsBoards::fh2();
        let stats: Vec<(i32, &str, Option<i64>)> = t
            .stats()
            .map(|b| (b.score_type, b.ambient_name.as_str(), b.max))
            .collect();
        assert_eq!(
            stats,
            [
                (7, "PhotoChallenge", Some(354)),
                (8, "BarnFinds", Some(10)),
                (9, "DiscountBoard", Some(150)),
                (10, "RoadDiscovery", Some(315)),
                (11, "RaceEncounter", None),
                (12, "BucketList", Some(55)),
                (13, "SkillsCombo", None),
                (14, "IslandRoadDiscovery", Some(50)),
                (15, "IslandDiscountBoard", Some(25)),
            ]
        );
        // 4/7's ceiling is the catalog's car models.
        assert_eq!(
            t.board(4, 7).unwrap().max,
            Some(crate::Catalog::fh2().cars().count() as i64)
        );
        assert_eq!(t.board(9, 5).unwrap().max, None);
        assert!(t.board(4, 5).is_none());
        assert!(
            StatsBoards::from_json(
                r#"{"boards":[{"scoreboardType":4,"scoreType":7,"ambientName":"x","max":-1}]}"#
            )
            .is_err()
        );
    }
}
