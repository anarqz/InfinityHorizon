//! DTOs of `/saves/v1` (docs/contracts/UNIVERSE_API.md; platform v1, ADR 0016): the nearby query
//! and the additive progress fact (the leaderboards, rivals and ghosts of this API were retired on
//! 2026-10-27: the game's own are the Forza RPC's, GAME_PLATFORM.md §3a). Mirror:
//! `packages/contracts/src/universe.ts` (the parity test below compares the field names of every
//! body). Routes: [`crate::routes::universe`].
//!
//! Query structs refuse unknown parameters; `mode` is optional on every read and, when present,
//! must be the ticket's mode (`PORTAL.MODE_MISMATCH`). Values are integers: times in
//! milliseconds, points, map coordinates.

use serde::{Deserialize, Serialize};

/// `kind` of a progress fact: GAME_PLATFORM.md §2.2's kinds but `score` and `ghost` (the game
/// writes those through `Scoreboard.SubmitScore` and `UploadFile`), plus `position`.
pub const PROGRESS_KINDS: [&str; 7] = [
    "race_result",
    "xp",
    "position",
    "profile_sync",
    "tables_sync",
    "drivatar",
    "livery",
];
/// Largest nearby radius (map units).
pub const MAX_NEARBY_RADIUS: u32 = 5_000;
/// Largest nearby answer.
pub const MAX_NEARBY_LIMIT: u32 = 32;
/// Largest absolute map coordinate accepted (the map is a plane; the game's own range is smaller).
pub const MAX_COORDINATE: i64 = 1_000_000;
/// Positions older than this are not nearby.
pub const NEARBY_STALE_SECS: i64 = 120;

/// `GET /saves/v1/nearby` query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NearbyQuery {
    /// Map x.
    pub x: i64,
    /// Map y.
    pub y: i64,
    /// 1 to [`MAX_NEARBY_RADIUS`].
    pub radius: u32,
    /// 1 to [`MAX_NEARBY_LIMIT`] (default 16).
    #[serde(default)]
    pub limit: Option<u32>,
    /// The ticket's mode when given.
    #[serde(default)]
    pub mode: Option<String>,
}

/// One nearby player.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NearbyItem {
    /// XUID, decimal.
    pub xuid: String,
    /// The plain name (OD-120).
    pub gamertag: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// Map x.
    pub x: i64,
    /// Map y.
    pub y: i64,
    /// Rounded distance from the query point.
    pub distance: u32,
    /// The car they drive, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car: Option<u32>,
    /// When the position was reported.
    pub updated_at: String,
}

/// `GET /saves/v1/nearby` → 200: nearest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Nearby {
    /// Players within the radius, fresher than [`NEARBY_STALE_SECS`].
    pub items: Vec<NearbyItem>,
}

/// `POST /saves/v1/progress` body (header `X-Fh2-Request-Id` required).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgressFact {
    /// One of [`PROGRESS_KINDS`].
    pub kind: String,
    /// Kind-specific facts (EVENTS.md §3.3; the typed views below for the kinds the universe reads).
    pub data: serde_json::Map<String, serde_json::Value>,
}

/// `race_result` data the universe reads (other fields are kept in the fact, not read here).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RaceResultData {
    /// The event id (the board).
    pub event: String,
    /// The finishing time.
    pub time_ms: i64,
    /// The car, when reported.
    #[serde(default)]
    pub car: Option<u32>,
}

/// `position` data: updates `player_position`, never recorded as a progression event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PositionData {
    /// Map x.
    pub x: i64,
    /// Map y.
    pub y: i64,
    /// Degrees, 0-359.
    #[serde(default)]
    pub heading: Option<u16>,
    /// The car driven.
    #[serde(default)]
    pub car: Option<u32>,
}

/// A plausibility rule that refused a value (CR-L14; the fact is kept).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressSignal {
    /// The rule (`min_time`).
    pub rule: String,
    /// The field it judged (`timeMs`).
    pub field: String,
}

/// `POST /saves/v1/progress` → 201 (`recorded: true`) or 200 (a retry: `recorded: false`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressRecorded {
    /// `UUIDv5(profileId, X-Fh2-Request-Id)`.
    pub event_id: String,
    /// Whether this request wrote it.
    pub recorded: bool,
    /// When the first copy was received.
    pub recorded_at: String,
    /// `false` when a plausibility rule refused the value (the fact is kept).
    pub applied: bool,
    /// Why it was not applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<ProgressSignal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_refuse_unknown_parameters_and_facts_are_strict() {
        assert!(
            serde_json::from_str::<NearbyQuery>(r#"{"x":1,"y":2,"radius":1,"extra":"x"}"#).is_err()
        );
        let q: NearbyQuery = serde_json::from_str(r#"{"x":-5,"y":7,"radius":100}"#).unwrap();
        assert_eq!((q.x, q.y, q.limit), (-5, 7, None));
        assert!(serde_json::from_str::<PositionData>(r#"{"x":1,"y":2,"z":3}"#).is_err());
        let race: RaceResultData =
            serde_json::from_str(r#"{"event":"e1","timeMs":61000,"position":2}"#).unwrap();
        assert_eq!(
            race.time_ms, 61_000,
            "race facts keep fields the universe does not read"
        );
        let answer = serde_json::to_value(ProgressRecorded {
            event_id: "e".into(),
            recorded: true,
            recorded_at: "t".into(),
            applied: true,
            signal: None,
        })
        .unwrap();
        assert!(answer.get("signal").is_none());
    }
}
