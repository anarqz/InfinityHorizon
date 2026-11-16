//! `GET /internal/dashboard/v1/boards` and `/boards/{family}/{board}` (DASHBOARD_API.md §3, PD-2):
//! the game's leaderboards for the play dashboard, ranked exactly as the game's
//! `Scoreboard.GetRows` ranks them (`fi_db::dashboard::boards`, over `fi_db::scores`' ranking),
//! website-hidden rows omitted after ranking.
//!
//! The boards are the catalog's (`fi_gamedb::leaderboards`, the server's Monthly Rivals rotation
//! `game::proxy::universe::rivals`), each read as the game's GetRows reads it
//! (`game::proxy::universe::boards::board_query`):
//!
//! | family | key | board | read as |
//! | --- | --- | --- | --- |
//! | `speed-traps` | 9/5 | camera 1-35 | the event (camera id) |
//! | `speed-zones` | 9/6 | camera 36-55 | the event |
//! | `rivals` | 1/1 | `trackId` | the route (every event on the track) |
//! | `showcases` | 6/1 | Showcase `eventId` | the route of its track (`trackId` ≥ 0 in the game's read; each Showcase track is its event's alone) |
//! | `monthly-rivals` | 3/1 | Monthly event id, months 2026-01 to now | the event |
//! | `stats` | 4/7-4/15 | slug | the event 0 of the slug's score type |
//! | `xp` | 8/4 | `global` | the event 0 (the non-club XP read) |
//!
//! Scores are stored as the game submitted them (seconds for times, m/s for speeds, counts and
//! points as such), which are §3's units: [`in_unit`] is the identity. `car.class` is the letter of
//! the row's `car_class_id` (0 D, 1 C, 2 B, 3 A, 4 S1, 5 S2, 6 X, 7 R; the catalog's `CarClasses`),
//! the `class` filter's too; `car.pi` is the display PI (608 for 0.608), held to the game's shown
//! range.

use super::{Dash, DashQuery, game, query, required_mode, unknown_game};
use crate::errors::ApiError;
use crate::game::proxy::forza::Lang;
use crate::game::proxy::universe::rivals;
use crate::views::at;
use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use fi_db::dashboard::{self, boards as db};
use fi_db::scores::{BoardFilter, lower_is_better};
use fi_domain::UnixMs;
use fi_gamedb::Catalog;
use fi_gamedb::leaderboards::{CameraKind, Leaderboards, Region};
use fi_protocol::ErrorCode;
use fi_protocol::dashboard::{
    BOARD_CLASSES, BOARDS_MAX_PAGE, BOARDS_PAGE_SIZE, BoardAssists, BoardBetter, BoardCar,
    BoardClassCount, BoardClub, BoardFamily, BoardIndex, BoardPage, BoardPlayer, BoardRegion,
    BoardRow, BoardSummary, BoardTop, BoardUnit,
};

/// A board family of §3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fam {
    /// 9/5.
    SpeedTraps,
    /// 9/6.
    SpeedZones,
    /// 1/1.
    Rivals,
    /// 6/1.
    Showcases,
    /// 3/1.
    MonthlyRivals,
    /// 4/7-4/15.
    Stats,
    /// 8/4.
    Xp,
}

/// The families in §3's order.
pub const FAMILIES: [Fam; 7] = [
    Fam::SpeedTraps,
    Fam::SpeedZones,
    Fam::Rivals,
    Fam::Showcases,
    Fam::MonthlyRivals,
    Fam::Stats,
    Fam::Xp,
];

/// The stats boards (4/7-4/15): slug and score type, as the game's submit path maps its ambient
/// activities (T-PD-S5, `fi_gamedb::stats_boards`, superseding T-PD-S3's enum + 5).
pub const STATS: [(&str, i32); 9] = [
    ("promo-cars", 7),
    ("barn-finds", 8),
    ("bonus-boards", 9),
    ("roads", 10),
    ("head-to-head", 11),
    ("bucket-list", 12),
    ("skill-chain", 13),
    ("island-roads", 14),
    ("island-boards", 15),
];

/// The stats board counted in points (best skill chain); every other stats board is a count.
const POINTS_STAT: i32 = 13;

/// The only XP board's slug.
const XP_GLOBAL: &str = "global";

impl Fam {
    /// The slug.
    pub fn slug(self) -> &'static str {
        match self {
            Fam::SpeedTraps => "speed-traps",
            Fam::SpeedZones => "speed-zones",
            Fam::Rivals => "rivals",
            Fam::Showcases => "showcases",
            Fam::MonthlyRivals => "monthly-rivals",
            Fam::Stats => "stats",
            Fam::Xp => "xp",
        }
    }

    /// The family of a slug.
    pub fn parse(slug: &str) -> Option<Fam> {
        FAMILIES.into_iter().find(|f| f.slug() == slug)
    }

    /// `(scoreboardType, scoreType)`; the stats family's first board for `Stats`.
    pub fn key(self) -> (i32, i32) {
        match self {
            Fam::SpeedTraps => (9, 5),
            Fam::SpeedZones => (9, 6),
            Fam::Rivals => (1, 1),
            Fam::Showcases => (6, 1),
            Fam::MonthlyRivals => (scores_monthly(), 1),
            Fam::Stats => (4, STATS[0].1),
            Fam::Xp => (8, 4),
        }
    }

    /// The family's unit (`stats`: count; its skill-chain board's page says points).
    pub fn unit(self) -> BoardUnit {
        match self {
            Fam::SpeedTraps | Fam::SpeedZones => BoardUnit::MetresPerSecond,
            Fam::Rivals | Fam::Showcases | Fam::MonthlyRivals => BoardUnit::Seconds,
            Fam::Stats => BoardUnit::Count,
            Fam::Xp => BoardUnit::Xp,
        }
    }

    /// Which way the game ranks it ([`lower_is_better`] of its score type).
    pub fn better(self) -> BoardBetter {
        if lower_is_better(self.key().1) {
            BoardBetter::Lower
        } else {
            BoardBetter::Higher
        }
    }

    /// Rows carry a car.
    fn cars(self) -> bool {
        !matches!(self, Fam::Stats | Fam::Xp)
    }

    /// A time board: rows carry the clean flag and the assists (the rivals screen's).
    fn timed(self) -> bool {
        matches!(self, Fam::Rivals | Fam::Showcases | Fam::MonthlyRivals)
    }

    /// Takes the `class` filter (rivals and Showcases).
    pub fn classed(self) -> bool {
        matches!(self, Fam::Rivals | Fam::Showcases)
    }
}

fn scores_monthly() -> i32 {
    fi_db::scores::MONTHLY_BOARD
}

/// A score as stored, in the board's §3 unit: the game submits seconds for times, m/s for speeds
/// and plain counts and points, and `game_score` keeps them as submitted, so this is the
/// identity (named so a future unit change has one place).
pub fn in_unit(_unit: BoardUnit, stored: f64) -> f64 {
    stored
}

/// The class id of a `class` filter value (`D` 0 to `X` 6).
pub fn class_id(text: &str) -> Option<i16> {
    BOARD_CLASSES
        .iter()
        .position(|c| *c == text)
        .and_then(|i| i16::try_from(i).ok())
}

/// The letter of a row's `car_class_id` (the catalog's `CarClasses`: 0 D ... 6 X, 7 R); an id the
/// catalog lacks falls back to the car's stock class, else the number.
pub fn class_name(catalog: &Catalog, car: i32, class: i16) -> String {
    catalog
        .class(i64::from(class))
        .or_else(|| {
            catalog
                .car(i64::from(car))
                .and_then(|c| catalog.class(c.class_id))
        })
        .map_or_else(|| class.to_string(), |c| c.name.clone())
}

fn region(r: Region) -> BoardRegion {
    match r {
        Region::Mainland => BoardRegion::Mainland,
        Region::StormIsland => BoardRegion::StormIsland,
    }
}

fn summary(board: String, title: String) -> BoardSummary {
    BoardSummary {
        board,
        title,
        region: None,
        disciplines: None,
        championships: None,
        length_m: None,
        entries: 0,
        top: None,
        refs: None,
    }
}

fn non_empty(v: &[String]) -> Option<Vec<String>> {
    (!v.is_empty()).then(|| v.to_vec())
}

/// One board of the catalog: its summary (no counts), how the game reads it, its key in the
/// family's grouped index read and its unit.
#[derive(Clone, Debug)]
pub struct Board {
    /// The summary, `entries` 0 and no `top`.
    pub summary: BoardSummary,
    /// The game's read.
    pub one: db::OneBoard,
    /// The value of the family's board column (track, event or score type).
    pub key: i32,
    /// The board's unit.
    pub unit: BoardUnit,
}

/// The Monthly Rivals month the server is in.
pub fn current_month() -> i64 {
    rivals::month_of(UnixMs::now_system())
}

fn camera_board(fam: Fam, c: &fi_gamedb::leaderboards::Camera) -> Board {
    let (sbt, st) = fam.key();
    let mut s = summary(c.id.to_string(), c.name.clone());
    s.region = Some(region(c.region));
    Board {
        summary: s,
        one: db::OneBoard {
            scoreboard_type: sbt,
            score_type: st,
            filter: BoardFilter::Event(c.id),
        },
        key: c.id,
        unit: fam.unit(),
    }
}

fn monthly_board(e: &rivals::RivalEvent) -> Option<Board> {
    let id = i32::try_from(e.id).ok()?;
    let (title, _, _) = rivals::texts(Catalog::fh2(), e, Lang::En);
    let mut s = summary(id.to_string(), title);
    s.length_m = i32::try_from(e.track)
        .ok()
        .and_then(|t| Leaderboards::fh2().track(t))
        .map(|t| t.length_m);
    Some(Board {
        summary: s,
        one: db::OneBoard {
            scoreboard_type: scores_monthly(),
            score_type: 1,
            filter: BoardFilter::Event(id),
        },
        key: id,
        unit: BoardUnit::Seconds,
    })
}

fn stat_board(slug: &str, score_type: i32) -> Board {
    Board {
        summary: summary(slug.to_owned(), slug.to_owned()),
        one: db::OneBoard {
            scoreboard_type: 4,
            score_type,
            filter: BoardFilter::Event(0),
        },
        key: score_type,
        unit: if score_type == POINTS_STAT {
            BoardUnit::Points
        } else {
            BoardUnit::Count
        },
    }
}

fn xp_board() -> Board {
    Board {
        summary: summary(XP_GLOBAL.to_owned(), XP_GLOBAL.to_owned()),
        one: db::OneBoard {
            scoreboard_type: 8,
            score_type: 4,
            filter: BoardFilter::Event(0),
        },
        key: 0,
        unit: BoardUnit::Xp,
    }
}

/// Every board of a family, in the index's order (Monthly Rivals: newest month first, months
/// 2026-01 to `month`).
pub fn family_boards(fam: Fam, month: i64) -> Vec<Board> {
    let l = Leaderboards::fh2();
    let (sbt, st) = fam.key();
    match fam {
        Fam::SpeedTraps | Fam::SpeedZones => {
            let kind = if fam == Fam::SpeedTraps {
                CameraKind::SpeedTrap
            } else {
                CameraKind::SpeedZone
            };
            l.cameras()
                .filter(|c| c.kind == kind)
                .map(|c| camera_board(fam, c))
                .collect()
        }
        Fam::Rivals => l
            .tracks()
            .map(|t| {
                let mut s = summary(t.id.to_string(), t.name.clone());
                s.disciplines = non_empty(&t.disciplines);
                s.championships = non_empty(&t.championships);
                s.length_m = Some(t.length_m);
                Board {
                    summary: s,
                    one: db::OneBoard {
                        scoreboard_type: sbt,
                        score_type: st,
                        filter: BoardFilter::Route(t.id),
                    },
                    key: t.id,
                    unit: BoardUnit::Seconds,
                }
            })
            .collect(),
        Fam::Showcases => l
            .showcases()
            .map(|sc| {
                let mut s = summary(sc.event_id.to_string(), sc.name.clone());
                s.length_m = l.track(sc.track_id).map(|t| t.length_m);
                Board {
                    summary: s,
                    one: db::OneBoard {
                        scoreboard_type: sbt,
                        score_type: st,
                        filter: BoardFilter::Route(sc.track_id),
                    },
                    key: sc.track_id,
                    unit: BoardUnit::Seconds,
                }
            })
            .collect(),
        Fam::MonthlyRivals => (0..=month)
            .rev()
            .flat_map(|m| rivals::month_events(Catalog::fh2(), m))
            .filter_map(|e| monthly_board(&e))
            .collect(),
        Fam::Stats => STATS.iter().map(|(s, t)| stat_board(s, *t)).collect(),
        Fam::Xp => vec![xp_board()],
    }
}

/// A board id as the URL carries it: digits only, no sign, no leading zero.
fn numeric(board: &str) -> Option<i32> {
    let n: i32 = board.parse().ok()?;
    (board.bytes().all(|b| b.is_ascii_digit()) && n.to_string() == board).then_some(n)
}

/// The catalog board `board` of `fam` (`None`: not found).
pub fn find_board(fam: Fam, board: &str, month: i64) -> Option<Board> {
    let l = Leaderboards::fh2();
    match fam {
        Fam::SpeedTraps | Fam::SpeedZones => {
            let c = l.camera(numeric(board)?)?;
            let kind = if fam == Fam::SpeedTraps {
                CameraKind::SpeedTrap
            } else {
                CameraKind::SpeedZone
            };
            (c.kind == kind).then(|| camera_board(fam, c))
        }
        Fam::Rivals | Fam::Showcases => {
            let id = numeric(board)?;
            family_boards(fam, month)
                .into_iter()
                .find(|b| b.summary.board == id.to_string())
        }
        Fam::MonthlyRivals => {
            let e = rivals::event(Catalog::fh2(), i64::from(numeric(board)?))?;
            (e.month <= month).then(|| monthly_board(&e)).flatten()
        }
        Fam::Stats => STATS
            .iter()
            .find(|(s, _)| *s == board)
            .map(|(s, t)| stat_board(s, *t)),
        Fam::Xp => (board == XP_GLOBAL).then(xp_board),
    }
}

/// The index read of a family: one grouped statement.
pub(crate) fn family_read(fam: Fam, keys: &[i32]) -> db::Family<'_> {
    let (sbt, st) = fam.key();
    match fam {
        Fam::Rivals | Fam::Showcases => db::Family::Tracks {
            scoreboard_type: sbt,
            score_type: st,
            tracks: keys,
        },
        Fam::Stats => db::Family::ScoreTypes {
            scoreboard_type: sbt,
            score_types: keys,
            event: 0,
        },
        Fam::SpeedTraps | Fam::SpeedZones | Fam::MonthlyRivals | Fam::Xp => db::Family::Events {
            scoreboard_type: sbt,
            score_type: st,
            events: keys,
        },
    }
}

pub(crate) fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn top(unit: BoardUnit, t: Option<(String, f64)>) -> Option<BoardTop> {
    t.map(|(name, score)| BoardTop {
        name,
        score: in_unit(unit, score),
        ref_: None,
    })
}

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::BoardsNotFound, "no such board")
}

/// `GET /internal/dashboard/v1/boards?game&mode`: 200 [`BoardIndex`] (every catalog board, empty
/// ones with `entries` 0); 400 `REQUEST.INVALID` `mode` or `game`. One grouped statement per family.
pub(super) async fn index(
    State(s): State<Dash>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<BoardIndex>, ApiError> {
    let q = query(q)?;
    let mode = required_mode(&q)?;
    let game = game(&q);
    let month = current_month();
    let mut conn = s.pool.acquire().await?;
    if !dashboard::game_known_in(&mut conn, &game).await? {
        return Err(unknown_game());
    }
    let mut families = Vec::with_capacity(FAMILIES.len());
    for fam in FAMILIES {
        let boards = family_boards(fam, month);
        let keys: Vec<i32> = boards.iter().map(|b| b.key).collect();
        let stats = db::family_stats_in(&mut conn, &game, &mode, &family_read(fam, &keys)).await?;
        let boards = boards
            .into_iter()
            .map(|b| {
                let mut summary = b.summary;
                if let Some(st) = stats.iter().find(|st| st.board == b.key) {
                    summary.entries = count(st.entries);
                    summary.top = top(b.unit, st.top.clone());
                }
                summary
            })
            .collect();
        families.push(BoardFamily {
            family: fam.slug().to_owned(),
            unit: fam.unit(),
            better: fam.better(),
            boards,
        });
    }
    Ok(Json(BoardIndex { families }))
}

/// `page`, default 1, 1 to [`BOARDS_MAX_PAGE`].
fn board_page_of(q: &DashQuery) -> Result<u32, ApiError> {
    match q.page.as_deref().map(str::trim) {
        None => Ok(1),
        Some(text) => text
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=BOARDS_MAX_PAGE).contains(n))
            .ok_or_else(|| ApiError::invalid("page", "page is 1 to 40")),
    }
}

/// A score row's car: the catalog's name ("Car {id}" for a car the base catalog lacks) and the PI
/// the game shows for the row's raw `car_pi` ([`fi_gamedb::pi::display_of`], T-PD-S6: the 2000GT's
/// raw 0.329861 is 404, never 330), with the class letter of that same band
/// ([`fi_gamedb::pi::class_of`]) so class and PI always agree. A row without a raw PI (0 or not a
/// number) shows the row's `car_class_id` letter and the catalog car's stock PI (0 without one).
pub(crate) fn board_car(car_id: i32, car_class_id: i16, car_pi: f32) -> BoardCar {
    let catalog = Catalog::fh2();
    let raw = f64::from(car_pi);
    let (class, pi) = if raw.is_finite() && raw > 0.0 {
        let band = i16::try_from(fi_gamedb::pi::class_of(raw)).unwrap_or(car_class_id);
        (
            class_name(catalog, car_id, band),
            fi_gamedb::pi::display_of(raw),
        )
    } else {
        (
            class_name(catalog, car_id, car_class_id),
            catalog
                .car(i64::from(car_id))
                .map_or(0, fi_gamedb::pi::catalog_display),
        )
    };
    BoardCar {
        id: car_id,
        name: car_name(catalog, car_id),
        class,
        pi,
    }
}

/// The catalog's name of a car ("Car {id}" for a car the base catalog lacks).
pub(crate) fn car_name(catalog: &Catalog, car_id: i32) -> String {
    catalog
        .car(i64::from(car_id))
        .map_or_else(|| format!("Car {car_id}"), |c| c.name())
}

/// The wire row of a listed score row of `fam`.
pub fn row(fam: Fam, unit: BoardUnit, r: db::WebRow) -> BoardRow {
    let a = r.assists;
    BoardRow {
        rank: count(r.rank),
        player: BoardPlayer {
            name: r.name,
            discord_name: r.discord_name,
            avatar: r.avatar,
            club: r.club_tag.map(|tag| BoardClub { tag }),
            ref_: None,
        },
        score: in_unit(unit, r.score),
        car: fam
            .cars()
            .then(|| board_car(r.car_id, r.car_class_id, r.car_pi)),
        clean: fam.timed().then_some(a.is_clean),
        assists: fam.timed().then_some(BoardAssists {
            abs: a.used_abs,
            tcs: a.used_tcs,
            stm: a.used_stm,
            auto_brake: a.used_auto_brake,
            auto_shift: a.used_auto_shifting,
            clutch: a.used_clutch,
        }),
        at: at(r.submitted_at),
    }
}

/// `GET /internal/dashboard/v1/boards/{family}/{board}?game&mode&class&page`: 200 [`BoardPage`];
/// 400 `REQUEST.INVALID` `mode`, `page` (1 to 40), `class` (rivals and Showcases only, `D` to `X`)
/// or `game`; 404 `BOARDS.NOT_FOUND` for a family or board the catalog does not have.
pub(super) async fn page(
    State(s): State<Dash>,
    Path((family, board)): Path<(String, String)>,
    q: Result<Query<DashQuery>, QueryRejection>,
) -> Result<Json<BoardPage>, ApiError> {
    let q = query(q)?;
    let mode = required_mode(&q)?;
    let page = board_page_of(&q)?;
    let month = current_month();
    let fam = Fam::parse(&family).ok_or_else(not_found)?;
    let b = find_board(fam, &board, month).ok_or_else(not_found)?;
    let class = match q.class.as_deref() {
        None => None,
        Some(text) if fam.classed() => Some(
            class_id(text.trim()).ok_or_else(|| ApiError::invalid("class", "class is D to X"))?,
        ),
        Some(_) => {
            return Err(ApiError::invalid(
                "class",
                "class applies to rivals and showcases only",
            ));
        }
    };
    let game = game(&q);
    let mut conn = s.pool.acquire().await?;
    if !dashboard::game_known_in(&mut conn, &game).await? {
        return Err(unknown_game());
    }
    let counts = db::board_counts_in(&mut conn, &game, &mode, &b.one).await?;
    let size = i64::from(BOARDS_PAGE_SIZE);
    let (total, rows) = db::board_rows_in(
        &mut conn,
        &game,
        &mode,
        &b.one,
        class,
        (i64::from(page) - 1) * size,
        size,
    )
    .await?;
    let mut summary = b.summary;
    summary.entries = count(counts.entries);
    summary.top = top(b.unit, counts.top);
    let classes = fam.classed().then(|| {
        BOARD_CLASSES
            .iter()
            .map(|c| BoardClassCount {
                class: (*c).to_owned(),
                entries: class_id(c)
                    .and_then(|id| counts.classes.iter().find(|(k, _)| *k == id))
                    .map_or(0, |(_, n)| count(*n)),
            })
            .collect()
    });
    Ok(Json(BoardPage {
        family: fam.slug().to_owned(),
        unit: b.unit,
        better: fam.better(),
        board: summary,
        classes,
        total: count(total),
        page,
        rows: rows.into_iter().map(|r| row(fam, b.unit, r)).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_match_the_contract_and_the_games_order() {
        let slugs: Vec<&str> = FAMILIES.iter().map(|f| f.slug()).collect();
        assert_eq!(
            slugs,
            vec![
                "speed-traps",
                "speed-zones",
                "rivals",
                "showcases",
                "monthly-rivals",
                "stats",
                "xp"
            ]
        );
        // §3's Better column equals the game's rule (score type 1 is a time).
        for (fam, better) in [
            (Fam::SpeedTraps, BoardBetter::Higher),
            (Fam::SpeedZones, BoardBetter::Higher),
            (Fam::Rivals, BoardBetter::Lower),
            (Fam::Showcases, BoardBetter::Lower),
            (Fam::MonthlyRivals, BoardBetter::Lower),
            (Fam::Stats, BoardBetter::Higher),
            (Fam::Xp, BoardBetter::Higher),
        ] {
            assert_eq!(fam.better(), better, "{fam:?}");
            assert_eq!(Fam::parse(fam.slug()), Some(fam));
        }
        for (_, st) in STATS {
            assert!(!lower_is_better(st));
        }
        // Every family's wire key is one the catalog names.
        let keys = Leaderboards::fh2().family_keys();
        for fam in FAMILIES {
            let (sbt, st) = fam.key();
            assert!(keys.contains(&format!("{sbt}/{st}")), "{fam:?}");
        }
        // The stats boards are T-PD-S5's (fi_gamedb::stats_boards), not the old taxonomy's.
        let traced: Vec<i32> = fi_gamedb::stats_boards::StatsBoards::fh2()
            .stats()
            .map(|b| b.score_type)
            .collect();
        assert_eq!(STATS.map(|(_, st)| st).to_vec(), traced);
        assert_eq!(
            STATS
                .iter()
                .find(|(s, _)| *s == "barn-finds")
                .map(|(_, t)| *t),
            Some(8)
        );
        assert_eq!(
            fi_gamedb::stats_boards::StatsBoards::fh2()
                .board(4, POINTS_STAT)
                .map(|b| b.ambient_name.as_str()),
            Some("SkillsCombo")
        );
        assert_eq!(Fam::parse("drift-zones"), None);
    }

    #[test]
    fn the_catalog_boards() {
        let month = rivals::month_of(UnixMs::from_millis(1_793_491_200_000)); // 2026-11-01
        assert_eq!(month, 10);
        let traps = family_boards(Fam::SpeedTraps, month);
        assert_eq!(traps.len(), 35);
        assert_eq!(traps[0].summary.title, "South San Giovanni Speed Trap");
        assert_eq!(traps[0].summary.region, Some(BoardRegion::Mainland));
        assert_eq!(traps[30].summary.region, Some(BoardRegion::StormIsland));
        assert_eq!(traps[0].one.filter, BoardFilter::Event(1));
        assert_eq!(family_boards(Fam::SpeedZones, month).len(), 20);
        let rivals = family_boards(Fam::Rivals, month);
        assert_eq!(rivals.len(), 287);
        assert!(rivals.iter().all(|b| b.summary.length_m.is_some()));
        let showcases = family_boards(Fam::Showcases, month);
        assert_eq!(
            showcases
                .iter()
                .map(|b| (b.summary.board.as_str(), b.one.filter))
                .collect::<Vec<_>>(),
            vec![
                ("92", BoardFilter::Route(578)),
                ("210", BoardFilter::Route(902)),
                ("560", BoardFilter::Route(910)),
                ("561", BoardFilter::Route(911)),
                ("562", BoardFilter::Route(912)),
            ]
        );
        let monthly = family_boards(Fam::MonthlyRivals, month);
        assert_eq!(monthly.len(), 11 * 12);
        // Newest first: November 2026's first event.
        assert_eq!(monthly[0].summary.board, (1000 + 10 * 16).to_string());
        assert!(monthly[0].summary.title.contains("Rival"));
        assert_eq!(family_boards(Fam::Stats, month).len(), 9);
        assert_eq!(family_boards(Fam::Xp, month).len(), 1);
    }

    #[test]
    fn boards_are_found_only_in_the_catalog() {
        let month = 10;
        assert!(find_board(Fam::SpeedTraps, "1", month).is_some());
        assert!(find_board(Fam::SpeedTraps, "36", month).is_none(), "a zone");
        assert!(find_board(Fam::SpeedZones, "36", month).is_some());
        for bad in ["01", "+1", "-1", "", "1.0", "x", "99999999999"] {
            assert!(find_board(Fam::SpeedTraps, bad, month).is_none(), "{bad}");
        }
        assert!(find_board(Fam::Rivals, "560", month).is_some());
        assert!(find_board(Fam::Rivals, "99999", month).is_none());
        let sc = find_board(Fam::Showcases, "92", month).unwrap();
        assert_eq!(sc.summary.title, "TRAIN IN VAIN");
        assert!(
            find_board(Fam::Showcases, "578", month).is_none(),
            "a track id"
        );
        assert!(find_board(Fam::MonthlyRivals, "1160", month).is_some());
        assert!(
            find_board(Fam::MonthlyRivals, "1176", month).is_none(),
            "next month"
        );
        assert!(find_board(Fam::MonthlyRivals, "999", month).is_none());
        let chain = find_board(Fam::Stats, "skill-chain", month).unwrap();
        assert_eq!((chain.unit, chain.one.score_type), (BoardUnit::Points, 13));
        assert_eq!(
            find_board(Fam::Stats, "roads", month).unwrap().unit,
            BoardUnit::Count
        );
        assert!(find_board(Fam::Stats, "drift", month).is_none());
        assert!(find_board(Fam::Xp, "global", month).is_some());
        assert!(find_board(Fam::Xp, "club", month).is_none());
    }

    #[test]
    fn classes_map_car_class_ids() {
        let ids: Vec<Option<i16>> = BOARD_CLASSES.iter().map(|c| class_id(c)).collect();
        assert_eq!(ids, (0..7).map(Some).collect::<Vec<_>>());
        assert_eq!(class_id("R"), None);
        assert_eq!(class_id("s1"), None);
        let c = Catalog::fh2();
        for (id, name) in BOARD_CLASSES.iter().enumerate() {
            assert_eq!(class_name(c, 247, id as i16), *name);
        }
        assert_eq!(class_name(c, 247, 7), "R");
        // An id the catalog lacks: the car's stock class (the 2000 GT is D), else the number.
        assert_eq!(class_name(c, 247, 99), "D");
        assert_eq!(class_name(c, 30_000, 99), "99");
    }

    #[test]
    fn scores_are_already_in_the_contract_units() {
        assert_eq!(in_unit(BoardUnit::Seconds, 93.25), 93.25);
        assert_eq!(in_unit(BoardUnit::MetresPerSecond, 88.125), 88.125);
        assert_eq!(in_unit(BoardUnit::Points, 275_000.0), 275_000.0);
    }

    #[test]
    fn a_row_shows_car_class_pi_and_assists_by_family() {
        let r = db::WebRow {
            rank: 4,
            profile_id: uuid::Uuid::nil(),
            name: "Driver".into(),
            discord_name: Some("Disc".into()),
            avatar: true,
            club_tag: Some("AB".into()),
            score: 93.25,
            submitted_at: UnixMs::from_millis(1_792_000_000_123),
            car_id: 247,
            car_class_id: 3,
            car_pi: 0.608,
            assists: fi_db::scores::Assists {
                is_clean: true,
                used_abs: true,
                ..Default::default()
            },
        };
        let timed = row(Fam::Rivals, BoardUnit::Seconds, r.clone());
        assert_eq!(timed.rank, 4);
        let car = timed.car.unwrap();
        // Raw 0.608 is B 690 (T-PD-S6), whatever class the row carries.
        assert_eq!(
            (car.name.as_str(), car.class.as_str(), car.pi),
            ("Toyota 2000 GT 1969", "B", 690)
        );
        // The 2000GT's own raw is D 404, never 330; X is 999; no raw: the stock D 404.
        assert_eq!(board_car(247, 0, 0.329861).pi, 404);
        let x = board_car(247, 6, 1.0);
        assert_eq!((x.class.as_str(), x.pi), ("X", 999));
        let none = board_car(247, 0, 0.0);
        assert_eq!((none.class.as_str(), none.pi), ("D", 404));
        assert_eq!(timed.clean, Some(true));
        assert!(timed.assists.unwrap().abs);
        assert_eq!(timed.player.club.unwrap().tag, "AB");
        let trap = row(Fam::SpeedTraps, BoardUnit::MetresPerSecond, r.clone());
        assert!(trap.car.is_some() && trap.clean.is_none() && trap.assists.is_none());
        let xp = row(Fam::Xp, BoardUnit::Xp, r);
        assert!(xp.car.is_none() && xp.clean.is_none());
    }
}
