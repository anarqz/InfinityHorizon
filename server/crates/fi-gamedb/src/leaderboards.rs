//! Forza Horizon 2's leaderboard taxonomy (T-PD-S3; DASHBOARD_API.md §3): `fh2/leaderboards.json`,
//! read by SAURON only from the game's files (docs/web/PLAY_LEADERBOARDS_METHOD.md; private, OD-32,
//! never shipped). [`Leaderboards::fh2`] parses it once and keeps what the play dashboard's boards
//! name: the 55 speed cameras (traps 1-35 on 9/5, zones 36-55 on 9/6; 31-35 and 51-55 on Storm
//! Island), the 287 tracks (name, the race modes and championships of the events that use them,
//! length) and the 5 Showcase events (6/1). Car names, classes and PI are [`crate::Catalog`]'s.
//!
//! Camera names are stored upper case in the game ("SOUTH SAN GIOVANNI SPEED TRAP"); [`Camera::name`]
//! is their title case ([`title_case`]). Showcase and track names are kept as the catalog has them.

use serde::Deserialize;
use std::collections::BTreeMap;

/// The game data file of the taxonomy (`FI_GAME_DATA_DIR`; never in the crate).
pub const LEADERBOARDS_FILE: &str = "fh2/leaderboards.json";

static FH2: fi_gamedata::Lazy<Leaderboards> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(LEADERBOARDS_FILE, Leaderboards::from_json));

/// Where a speed camera stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// The base game's map.
    Mainland,
    /// The Storm Island expansion.
    StormIsland,
}

/// A speed camera's board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraKind {
    /// A speed trap (9/5: the speed through the camera).
    SpeedTrap,
    /// A speed zone (9/6: the average speed through the zone).
    SpeedZone,
}

/// One speed camera (`speed_cameras.xml`, `speed_cameras_island.xml`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Camera {
    /// `LeaderboardID`: the board's `gameScoreboardId` (1-55).
    pub id: i32,
    /// Trap or zone.
    pub kind: CameraKind,
    /// The English name, title case ("South San Giovanni Speed Trap").
    pub name: String,
    /// Mainland or Storm Island.
    pub region: Region,
}

/// One track as the boards show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardTrack {
    /// `Tracks.id`, the route board's `trackId`.
    pub id: i32,
    /// The English name.
    pub name: String,
    /// The race modes (`RaceModes.Enum`) of the events that use it ("StreetRace", ...); empty for a
    /// track no event uses.
    pub disciplines: Vec<String>,
    /// The championships with an event on it.
    pub championships: Vec<String>,
    /// Length, metres.
    pub length_m: i64,
}

/// One Showcase event (AnthemEvents.EventType 2; its board is 6/1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Showcase {
    /// The AnthemEventId (the scores' `gameScoreboardId`).
    pub event_id: i32,
    /// The English name as the catalog has it ("TRAIN IN VAIN").
    pub name: String,
    /// Its race's track (the scores' `trackId`).
    pub track_id: i32,
}

#[derive(Deserialize)]
struct RawFamily {
    key: String,
}

#[derive(Deserialize)]
struct RawCamera {
    id: i32,
    kind: String,
    name: String,
    region: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTrack {
    track_id: i32,
    name: String,
    length_m: i64,
    #[serde(default)]
    disciplines: Vec<String>,
    #[serde(default)]
    championships: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRace {
    track_id: i32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawShowcase {
    event_id: i32,
    name: String,
    races: Vec<RawRace>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    families: Vec<RawFamily>,
    speed_cameras: Vec<RawCamera>,
    tracks: Vec<RawTrack>,
    showcases: Vec<RawShowcase>,
}

/// Why the taxonomy JSON was refused.
#[derive(Debug)]
pub enum LeaderboardsError {
    /// Not the extractor's shape.
    Json(serde_json::Error),
    /// A value outside what the boards know (an unknown camera kind or region, a Showcase
    /// without a race).
    Shape(String),
}

impl std::fmt::Display for LeaderboardsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaderboardsError::Json(e) => write!(f, "{e}"),
            LeaderboardsError::Shape(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for LeaderboardsError {}

/// The taxonomy, indexed.
#[derive(Clone, Debug, Default)]
pub struct Leaderboards {
    family_keys: Vec<String>,
    cameras: BTreeMap<i32, Camera>,
    tracks: BTreeMap<i32, BoardTrack>,
    showcases: BTreeMap<i32, Showcase>,
}

impl Leaderboards {
    /// The FH2 taxonomy of the game data ([`LEADERBOARDS_FILE`]), parsed once; panics with the
    /// game data's error ([`Leaderboards::try_fh2`]).
    pub fn fh2() -> &'static Leaderboards {
        &FH2
    }

    /// [`Leaderboards::fh2`], or why the game data has none.
    pub fn try_fh2() -> Result<&'static Leaderboards, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// A taxonomy from the extractor's JSON.
    pub fn from_json(text: &str) -> Result<Leaderboards, LeaderboardsError> {
        let raw: Raw = serde_json::from_str(text).map_err(LeaderboardsError::Json)?;
        // The stats boards (4/x) are T-PD-S5's traced mapping ([`crate::stats_boards`]), never this
        // file's inferred rows: its 4/x keys are left out and the traced ones put in their place.
        let mut family_keys: Vec<String> = raw
            .families
            .into_iter()
            .map(|f| f.key)
            .filter(|k| !k.starts_with("4/"))
            .collect();
        family_keys.extend(
            crate::stats_boards::StatsBoards::fh2()
                .stats()
                .map(|b| format!("4/{}", b.score_type)),
        );
        let mut out = Leaderboards {
            family_keys,
            ..Leaderboards::default()
        };
        for c in raw.speed_cameras {
            let kind = match c.kind.as_str() {
                "speed_trap" => CameraKind::SpeedTrap,
                "speed_zone" => CameraKind::SpeedZone,
                other => return Err(LeaderboardsError::Shape(format!("camera kind {other}"))),
            };
            let region = match c.region.as_str() {
                "mainland" => Region::Mainland,
                "storm_island" => Region::StormIsland,
                other => return Err(LeaderboardsError::Shape(format!("camera region {other}"))),
            };
            out.cameras.insert(
                c.id,
                Camera {
                    id: c.id,
                    kind,
                    name: title_case(&c.name),
                    region,
                },
            );
        }
        for t in raw.tracks {
            out.tracks.insert(
                t.track_id,
                BoardTrack {
                    id: t.track_id,
                    name: t.name,
                    disciplines: t.disciplines,
                    championships: t.championships,
                    length_m: t.length_m,
                },
            );
        }
        for s in raw.showcases {
            let track = s.races.first().map(|r| r.track_id).ok_or_else(|| {
                LeaderboardsError::Shape(format!("showcase {} has no race", s.event_id))
            })?;
            out.showcases.insert(
                s.event_id,
                Showcase {
                    event_id: s.event_id,
                    name: s.name,
                    track_id: track,
                },
            );
        }
        Ok(out)
    }

    /// The board families' wire keys the game uses (`scoreboardType/scoreType`, "9/5", ...;
    /// `storefront` for the creators' board), in the catalog's order, the stats boards (4/7 to
    /// 4/15, from [`crate::stats_boards`]) last.
    pub fn family_keys(&self) -> &[String] {
        &self.family_keys
    }

    /// A speed camera by `LeaderboardID`.
    pub fn camera(&self, id: i32) -> Option<&Camera> {
        self.cameras.get(&id)
    }

    /// Every speed camera, by id.
    pub fn cameras(&self) -> impl Iterator<Item = &Camera> {
        self.cameras.values()
    }

    /// A track.
    pub fn track(&self, id: i32) -> Option<&BoardTrack> {
        self.tracks.get(&id)
    }

    /// Every track, by id.
    pub fn tracks(&self) -> impl Iterator<Item = &BoardTrack> {
        self.tracks.values()
    }

    /// A Showcase by event id.
    pub fn showcase(&self, event_id: i32) -> Option<&Showcase> {
        self.showcases.get(&event_id)
    }

    /// Every Showcase, by event id.
    pub fn showcases(&self) -> impl Iterator<Item = &Showcase> {
        self.showcases.values()
    }
}

/// Title case of an upper-case game name: each word (split at spaces and hyphens) keeps its first
/// letter and lowers the rest ("EAST SAINT-MARTIN SPEED TRAP" → "East Saint-Martin Speed Trap").
pub fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut start = true;
    for c in text.chars() {
        if start {
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
        start = c == ' ' || c == '-';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_taxonomy_is_the_games() {
        let l = Leaderboards::fh2();
        // 55 cameras: traps 1-35, zones 36-55; 31-35 and 51-55 on Storm Island.
        assert_eq!(l.cameras().count(), 55);
        for c in l.cameras() {
            let trap = (1..=35).contains(&c.id);
            assert_eq!(c.kind == CameraKind::SpeedTrap, trap, "{}", c.id);
            assert!((1..=55).contains(&c.id));
            let island = (31..=35).contains(&c.id) || (51..=55).contains(&c.id);
            assert_eq!(c.region == Region::StormIsland, island, "{}", c.id);
            assert!(
                !c.name.is_empty() && c.name != c.name.to_uppercase(),
                "{}",
                c.name
            );
        }
        assert_eq!(
            l.camera(1).map(|c| c.name.as_str()),
            Some("South San Giovanni Speed Trap")
        );
        assert!(
            l.cameras()
                .any(|c| c.name == "East Saint-Martin Speed Trap")
        );
        // 287 tracks; Laguna Seca's full circuit; a raced route with its modes and championships.
        assert_eq!(l.tracks().count(), 287);
        let laguna = l.track(0).unwrap();
        assert_eq!(
            (laguna.name.as_str(), laguna.length_m),
            ("Full Circuit", 3602)
        );
        let castelletto = l.track(560).unwrap();
        assert_eq!(castelletto.disciplines, vec!["LapsRace".to_owned()]);
        assert!(castelletto.championships.len() > 3);
        // 5 Showcases, each on its own track.
        let ids: Vec<i32> = l.showcases().map(|s| s.event_id).collect();
        assert_eq!(ids, vec![92, 210, 560, 561, 562]);
        assert_eq!(l.showcase(92).map(|s| s.track_id), Some(578));
        for s in l.showcases() {
            assert!(l.track(s.track_id).is_some(), "{}", s.event_id);
        }
        // The families the boards serve are all in the catalog.
        let keys = l.family_keys();
        for key in [
            "1/1", "3/1", "6/1", "9/5", "9/6", "4/7", "4/8", "4/9", "4/10", "4/11", "4/12", "4/13",
            "4/14", "4/15", "8/4",
        ] {
            assert!(keys.iter().any(|k| k == key), "{key}");
        }
    }

    #[test]
    fn title_case_splits_at_spaces_and_hyphens() {
        assert_eq!(
            title_case("SOUTH SAN GIOVANNI SPEED TRAP"),
            "South San Giovanni Speed Trap"
        );
        assert_eq!(
            title_case("NORTH SAINT-MARTIN SPEED ZONE"),
            "North Saint-Martin Speed Zone"
        );
        assert_eq!(title_case(""), "");
    }

    #[test]
    fn a_bad_shape_is_refused() {
        let bad = r#"{"families":[],"speedCameras":[{"id":1,"kind":"drift","name":"X","region":"mainland"}],
            "tracks":[],"showcases":[]}"#;
        assert!(matches!(
            Leaderboards::from_json(bad),
            Err(LeaderboardsError::Shape(_))
        ));
        assert!(matches!(
            Leaderboards::from_json("{}"),
            Err(LeaderboardsError::Json(_))
        ));
    }
}
