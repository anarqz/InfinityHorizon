//! The in-game Statistics screen as the play dashboard shows it (DASHBOARD_API.md §4, PD-3,
//! OD-170): [`StatsLayout::fh2`] is `fh2/stats-layout.json`, built from the table of
//! docs/web/PLAY_STATS_SCREEN.md (SAURON's T-PD-S2: 8 tabs, 162 rows in display order) by
//! `tools/gamedata/fh2_stats_layout.py`, plus `fh2/stats-extras.json` (T-PD-S4: units, the
//! "n / total" denominators, the radio station names and the overall-completion formula).
//!
//! [`StatsLayout::evaluate`] reads a profile's stored `game_profile.data` (the merged
//! `OnlineProfile.UpdateAll` sections `compareStats[0]`, `miscStats[0]`, `playerRaceHistory[0]`,
//! `drivatarStats[0]`) and returns each tab's rows that have a value. OD-170: only stored values;
//! a row whose source is save-only or not decoded, whose value is absent or not a finite number,
//! or whose ratio divides by zero is left out, and a tab without rows is left out.
//!
//! Values are in the contract's units: seconds, metres, m/s, g, percent 0-100; tire load is sent
//! in newtons (stored in hectonewtons) and tire temperature in °C (stored in °F); a progress
//! percentage is shown at most 100 (the game's `min(value, 100)`); the overall completion is
//! rounded to one decimal as the game shows it. A `car` row's value is the car id, a `radio` row's
//! the station id.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The game data file of the layout (`FI_GAME_DATA_DIR`; never in the crate).
pub const STATS_LAYOUT_FILE: &str = "fh2/stats-layout.json";
/// The game data file of the T-PD-S4 extras (`FI_GAME_DATA_DIR`; never in the crate).
pub const STATS_EXTRAS_FILE: &str = "fh2/stats-extras.json";

static FH2: fi_gamedata::Lazy<StatsLayout> = fi_gamedata::Lazy::new(|| {
    let extras = fi_gamedata::parse(STATS_EXTRAS_FILE, |t| Ok::<_, String>(t.to_owned()))?;
    fi_gamedata::parse(STATS_LAYOUT_FILE, |layout| {
        StatsLayout::from_json(layout, &extras)
    })
});

/// The championships the overall completion counts against (`FUN_140c19cc0`; the formula's
/// `championshipsCompleted / 168`).
pub const CHAMPIONSHIPS: f64 = 168.0;

/// A stored value: `section[0].field` of `game_profile.data`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatPath {
    /// `compareStats`, `miscStats`, `playerRaceHistory` or `drivatarStats`.
    pub section: String,
    /// The field (`timeDriven`).
    pub field: String,
}

impl StatPath {
    /// `section[0].field` (the layout's) or `section.field` (the extras' completion terms).
    pub fn parse(text: &str) -> Option<StatPath> {
        let (section, field) = text.split_once("[0].").or_else(|| text.split_once('.'))?;
        let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
        (ok(section) && ok(field)).then(|| StatPath {
            section: section.to_owned(),
            field: field.to_owned(),
        })
    }

    /// The value in `data`, when stored as a finite number.
    pub fn read(&self, data: &Value) -> Option<f64> {
        data.get(&self.section)?
            .get(0)?
            .get(&self.field)?
            .as_f64()
            .filter(|v| v.is_finite())
    }
}

/// How a ratio row is computed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RatioKind {
    /// `a / b`.
    Quotient,
    /// `100 * a / b`.
    Percent,
    /// `100 * (a - b) / a`.
    Completion,
}

/// Where a row's value comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// One stored value.
    Path(StatPath),
    /// Two stored values.
    Ratio {
        /// How.
        kind: RatioKind,
        /// The numerator (the base of a completion).
        a: StatPath,
        /// The denominator (the quit count of a completion).
        b: StatPath,
    },
    /// The overall completion (T-PD-S4 formula).
    OverallCompletion,
    /// Not shown (OD-170): `save-only` or `computed` (not decoded).
    Omit(String),
}

/// One row of the screen.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutRow {
    /// The game's item index ("0x01").
    pub index: String,
    /// The game's string id ("IDS_Display_Time_Driven").
    pub ids: String,
    /// The stable key (unique in its tab).
    pub key: String,
    /// DASHBOARD_API.md §4 `StatFormat`.
    pub format: String,
    /// The game's ProfileData path(s) ("Main/TimeDriven").
    pub profile_paths: Vec<String>,
    /// The value's source.
    pub source: Source,
}

/// One tab.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutTab {
    /// `general`, `online`, ..., `storm-island`.
    pub tab: String,
    /// In display order.
    pub rows: Vec<LayoutRow>,
}

/// One evaluated row.
#[derive(Clone, Debug, PartialEq)]
pub struct StatValue {
    /// The row's key.
    pub key: String,
    /// Its format.
    pub format: String,
    /// The value in the contract's unit (a car id for `car`, a station id for `radio`).
    pub value: f64,
    /// `of`: the game's total.
    pub of: Option<i64>,
    /// `radio`: the station's name (none for id 0, "None", which is UI copy).
    pub station: Option<String>,
}

#[derive(Deserialize)]
struct RawRatio {
    kind: String,
    a: String,
    b: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRow {
    index: String,
    ids: String,
    key: String,
    format: String,
    #[serde(default)]
    profile_paths: Vec<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    ratio: Option<RawRatio>,
    #[serde(default)]
    computed: Option<String>,
    #[serde(default)]
    omit: bool,
    #[serde(default)]
    omit_reason: Option<String>,
}

#[derive(Deserialize)]
struct RawTab {
    tab: String,
    rows: Vec<RawRow>,
}

#[derive(Deserialize)]
struct RawLayout {
    tabs: Vec<RawTab>,
}

#[derive(Deserialize)]
struct RawTotal {
    total: i64,
}

#[derive(Deserialize)]
struct RawTotals {
    rows: BTreeMap<String, RawTotal>,
}

#[derive(Deserialize)]
struct RawWeight {
    value: f64,
}

#[derive(Deserialize)]
struct RawCompletion {
    formula: String,
    w: RawWeight,
    terms: Vec<(String, f64)>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawExtras {
    totals: RawTotals,
    radio_stations: BTreeMap<String, String>,
    overall_completion: RawCompletion,
}

/// The overall completion's inputs (T-PD-S4).
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    /// The championship weight `w`.
    pub weight: f64,
    /// Each discovery term and its total.
    pub terms: Vec<(StatPath, f64)>,
    /// The completed championships (`miscStats[0].carChampionshipsCompleted`).
    pub championships: StatPath,
}

impl Completion {
    /// `100 * ((1 - w) * 0.1 * Σ stat_i / total_i + w * championships / 168)`, one decimal; `None`
    /// when a term is not stored.
    pub fn of(&self, data: &Value) -> Option<f64> {
        let mut sum = 0.0;
        for (path, total) in &self.terms {
            sum += path.read(data)? / total;
        }
        let championships = self.championships.read(data)? / CHAMPIONSHIPS;
        let pct = 100.0 * ((1.0 - self.weight) * 0.1 * sum + self.weight * championships);
        Some((pct * 10.0).round() / 10.0)
    }
}

/// Why the layout or the extras were refused.
#[derive(Debug)]
pub struct StatsError(pub String);

impl std::fmt::Display for StatsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StatsError {}

/// The screen: tabs, the denominators, the station names, the completion formula.
#[derive(Clone, Debug, Default)]
pub struct StatsLayout {
    tabs: Vec<LayoutTab>,
    totals: BTreeMap<String, i64>,
    stations: BTreeMap<i64, String>,
    completion: Option<Completion>,
}

fn path(text: &str) -> Result<StatPath, StatsError> {
    StatPath::parse(text).ok_or_else(|| StatsError(format!("bad path {text}")))
}

impl StatsLayout {
    /// The FH2 screen of the game data ([`STATS_LAYOUT_FILE`] and [`STATS_EXTRAS_FILE`]), parsed
    /// once; panics with the game data's error ([`StatsLayout::try_fh2`]).
    pub fn fh2() -> &'static StatsLayout {
        &FH2
    }

    /// [`StatsLayout::fh2`], or why the game data has none.
    pub fn try_fh2() -> Result<&'static StatsLayout, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// The screen from the layout and extras JSON.
    pub fn from_json(layout: &str, extras: &str) -> Result<StatsLayout, StatsError> {
        let raw: RawLayout =
            serde_json::from_str(layout).map_err(|e| StatsError(format!("layout: {e}")))?;
        let ex: RawExtras =
            serde_json::from_str(extras).map_err(|e| StatsError(format!("extras: {e}")))?;
        let mut tabs = Vec::new();
        for t in raw.tabs {
            let mut rows = Vec::new();
            for r in t.rows {
                let source = if r.omit {
                    Source::Omit(r.omit_reason.unwrap_or_else(|| "omitted".into()))
                } else if r.computed.as_deref() == Some("overallCompletion") {
                    Source::OverallCompletion
                } else if let Some(ratio) = r.ratio {
                    let kind = match ratio.kind.as_str() {
                        "quotient" => RatioKind::Quotient,
                        "percent" => RatioKind::Percent,
                        "completion" => RatioKind::Completion,
                        other => return Err(StatsError(format!("ratio kind {other}"))),
                    };
                    Source::Ratio {
                        kind,
                        a: path(&ratio.a)?,
                        b: path(&ratio.b)?,
                    }
                } else if let Some(p) = r.path {
                    Source::Path(path(&p)?)
                } else {
                    return Err(StatsError(format!("row {} has no source", r.index)));
                };
                rows.push(LayoutRow {
                    index: r.index,
                    ids: r.ids,
                    key: r.key,
                    format: r.format,
                    profile_paths: r.profile_paths,
                    source,
                });
            }
            tabs.push(LayoutTab { tab: t.tab, rows });
        }
        let stations = ex
            .radio_stations
            .into_iter()
            .filter_map(|(k, v)| k.parse::<i64>().ok().map(|id| (id, v)))
            .collect();
        let oc = ex.overall_completion;
        if !oc.formula.contains("/ 168") {
            return Err(StatsError(
                "the completion formula no longer divides by 168".into(),
            ));
        }
        let completion = Completion {
            weight: oc.w.value,
            terms: oc
                .terms
                .iter()
                .map(|(p, total)| Ok((path(p)?, *total)))
                .collect::<Result<_, StatsError>>()?,
            championships: path("miscStats[0].carChampionshipsCompleted")?,
        };
        Ok(StatsLayout {
            tabs,
            totals: ex
                .totals
                .rows
                .into_iter()
                .map(|(k, v)| (k, v.total))
                .collect(),
            stations,
            completion: Some(completion),
        })
    }

    /// Every tab, in display order, omitted rows included.
    pub fn tabs(&self) -> &[LayoutTab] {
        &self.tabs
    }

    /// The game's total of an "n / total" row, by its ProfileData path.
    pub fn total(&self, profile_path: &str) -> Option<i64> {
        self.totals.get(profile_path).copied()
    }

    /// A radio station's name; `None` for id 0 ("None", UI copy) and unknown ids.
    pub fn station(&self, id: i64) -> Option<&str> {
        (id != 0).then(|| self.stations.get(&id).map(String::as_str))?
    }

    /// The overall-completion formula.
    pub fn completion(&self) -> Option<&Completion> {
        self.completion.as_ref()
    }

    fn value(&self, row: &LayoutRow, data: &Value) -> Option<f64> {
        let v = match &row.source {
            Source::Omit(_) => return None,
            Source::OverallCompletion => return self.completion.as_ref()?.of(data),
            Source::Path(p) => p.read(data)?,
            Source::Ratio { kind, a, b } => {
                let (a, b) = (a.read(data)?, b.read(data)?);
                match kind {
                    RatioKind::Quotient if b != 0.0 => a / b,
                    RatioKind::Percent if b != 0.0 => 100.0 * a / b,
                    RatioKind::Completion if a != 0.0 => 100.0 * (a - b) / a,
                    _ => return None,
                }
            }
        };
        let v = match (row.format.as_str(), &row.source) {
            ("load", _) => v * 100.0,
            ("temperature", _) => (v - 32.0) * 5.0 / 9.0,
            ("percent", Source::Path(_)) => v.min(100.0),
            ("car", _) if v <= 0.0 => return None,
            _ => v,
        };
        v.is_finite().then_some(v)
    }

    /// Each tab with at least one value in `data`, in display order.
    pub fn evaluate(&self, data: &Value) -> Vec<(String, Vec<StatValue>)> {
        let mut out = Vec::new();
        for t in &self.tabs {
            let rows: Vec<StatValue> = t
                .rows
                .iter()
                .filter_map(|r| {
                    let value = self.value(r, data)?;
                    Some(StatValue {
                        key: r.key.clone(),
                        format: r.format.clone(),
                        value,
                        of: (r.format == "of")
                            .then(|| r.profile_paths.first().and_then(|p| self.total(p)))
                            .flatten(),
                        station: (r.format == "radio")
                            .then(|| self.station(value as i64).map(str::to_owned))
                            .flatten(),
                    })
                })
                .collect();
            if !rows.is_empty() {
                out.push((t.tab.clone(), rows));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_follow_the_labels_and_the_units() {
        let l = StatsLayout::fh2();
        let get = |key: &str| {
            l.tabs()
                .iter()
                .flat_map(|t| &t.rows)
                .find(|r| r.key == key)
                .unwrap()
        };
        assert_eq!(get("garageValue").format, "credits");
        assert_eq!(get("totalWinnings").format, "credits");
        assert_eq!(get("numPodiums").format, "int");
        assert_eq!(get("totalXP").format, "int");
        assert_eq!(get("avagSpeed").format, "speed");
        assert_eq!(get("longestDrift").format, "distance");
        assert_eq!(get("barnFindsDiscovered").format, "of");
        assert!(matches!(
            get("onlineRaceCompletion").source,
            Source::Ratio {
                kind: RatioKind::Completion,
                ..
            }
        ));
        assert!(matches!(
            get("onlineWinPercentage").source,
            Source::Ratio {
                kind: RatioKind::Percent,
                ..
            }
        ));
        // Every "n / total" row of a stored source has its total.
        for r in l.tabs().iter().flat_map(|t| &t.rows) {
            if r.format == "of" && !matches!(r.source, Source::Omit(_)) {
                assert!(l.total(&r.profile_paths[0]).is_some(), "{}", r.key);
            }
        }
        assert_eq!(l.total("CompareStats/RoadsDiscovered"), Some(315));
        assert_eq!(l.station(1), Some("Horizon Pulse"));
        assert_eq!(l.station(0), None, "None is UI copy");
        assert_eq!(l.station(99), None);
        let c = l.completion().unwrap();
        assert_eq!((c.weight, c.terms.len()), (0.5, 10));
    }

    fn synthetic() -> Value {
        json!({
            "playerRaceHistory": [{"timeDriven": 10852, "distanceDriven": 273150, "numRaces": 40,
                "topSpeed": 88.5, "longestDrift": "NaN", "highestSkillCar": 247}],
            "miscStats": [{"totalCollisions": 100, "onlineRacesEntered": 0, "onlineRacesQuit": 0,
                "favouriteRadioStation": 3, "favouriteCar": 0, "carChampionshipsCompleted": 3,
                "onlineTeamRacesEntered": 4, "onlineTeamRacesWon": 1}],
            "compareStats": [{"barnFinds": 5, "discountBoardsSmashed": 30, "speedTrapsTriggered": 15,
                "speedZonesTriggered": 3, "carMeetsDiscovered": 6, "destinationsVisited": 0,
                "perksUnlocked": 5, "showcasesWon": 1, "bucketlistEventsCompleted": 11,
                "roadsDiscovered": 63, "horizonFinalProgress": 140, "onlineRacesWon": 2}],
            "drivatarStats": [{"maxLeftFrontTireLoad": 38.5, "maxLeftFrontTireTemp": 212.0}]
        })
    }

    #[test]
    fn stored_values_become_rows_and_the_rest_is_left_out() {
        let l = StatsLayout::fh2();
        let tabs = l.evaluate(&synthetic());
        let find = |tab: &str, key: &str| {
            tabs.iter()
                .find(|(t, _)| t == tab)
                .and_then(|(_, rows)| rows.iter().find(|r| r.key == key))
                .cloned()
        };
        let v = |tab: &str, key: &str| find(tab, key).map(|r| r.value);
        assert_eq!(v("general", "timeDriven"), Some(10852.0));
        // average = a / b; average speed = distance / time (m/s).
        assert_eq!(v("general", "avgNumberCollisionsInARace"), Some(2.5));
        let avg = v("records", "avagSpeed").unwrap();
        assert!((avg - 273150.0 / 10852.0).abs() < 1e-9);
        // "NaN" and absent values, and divisions by zero, are left out.
        assert_eq!(v("records", "longestDrift"), None);
        assert_eq!(v("online", "onlineWinPercentage"), None, "b = 0");
        assert_eq!(v("online", "onlineRaceCompletion"), None, "a = 0");
        assert_eq!(v("online", "teamRaceWinPercentage"), Some(25.0));
        assert_eq!(v("general", "garageValue"), None, "absent");
        // A car id 0 is no car; a real one stays an id.
        assert_eq!(v("general", "favoriteCar"), None);
        assert_eq!(v("skills", "highestSkillChainCar"), Some(247.0));
        // Radio: the station's name.
        let radio = find("general", "favoriteRadioStation").unwrap();
        assert_eq!(
            (radio.value, radio.station.as_deref()),
            (3.0, Some("Horizon XS"))
        );
        // n / total.
        let barns = find("discovery", "barnFindsDiscovered").unwrap();
        assert_eq!((barns.value, barns.of), (5.0, Some(10)));
        // Progress is shown at most 100.
        assert_eq!(v("single-player", "progressToHorizonFinal"), Some(100.0));
        // Tire load in newtons, temperature in °C.
        assert_eq!(v("drivatar", "maxLFTireLoad"), Some(3850.0));
        assert_eq!(v("drivatar", "maxLFTireTemp"), Some(100.0));
        // Overall completion: 100 * (0.5 * 0.1 * Σ + 0.5 * 3 / 168), one decimal.
        let sum = 5.0 / 10.0
            + 30.0 / 150.0
            + 15.0 / 30.0
            + 3.0 / 15.0
            + 6.0 / 6.0
            + 0.0 / 6.0
            + 5.0 / 25.0
            + 1.0 / 5.0
            + 11.0 / 55.0
            + 63.0 / 315.0;
        let expected = ((100.0 * (0.05 * sum + 0.5 * 3.0 / 168.0)) * 10.0_f64).round() / 10.0;
        assert_eq!(v("general", "overallCompletion"), Some(expected));
        // Storm Island is save-only: no tab.
        assert!(tabs.iter().all(|(t, _)| t != "storm-island"));
        // A missing completion term: no completion.
        let mut partial = synthetic();
        partial["compareStats"][0]
            .as_object_mut()
            .unwrap()
            .remove("roadsDiscovered");
        assert!(
            l.evaluate(&partial)
                .iter()
                .flat_map(|(_, r)| r)
                .all(|r| r.key != "overallCompletion")
        );
        // Nothing stored: no tabs.
        assert!(l.evaluate(&json!({})).is_empty());
    }

    #[test]
    fn paths_parse_strictly() {
        assert_eq!(
            StatPath::parse("compareStats[0].barnFinds"),
            StatPath::parse("compareStats.barnFinds")
        );
        assert!(StatPath::parse("compareStats[1].x").is_none());
        assert!(StatPath::parse("nofield").is_none());
    }
}
