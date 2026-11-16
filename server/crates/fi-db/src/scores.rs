//! The game's leaderboards, ghosts, rivals and XP (SER-M3 step 6; GAME_PLATFORM.md §3a "Supporter
//! universe"): `game_score`, `game_ghost`, `game_xp` (migration `20261019120000_scores.sql`), ported
//! from the Beta's `scores`, `ghosts` and `xp` (`deprecated/beta/server/src/db.rs`: `add_score`,
//! `leaderboard_of`, `is_personal_best`, `save_ghost`, `ghost_id`, `nearest_ghost`, `xp_since`).
//!
//! A board is the game's key ([`BoardKey`]); a profile keeps one row per key, its best
//! ([`lower_is_better`]: score type 1 is a time, every other a speed or points). A route board is
//! every event on a track, deduplicated to each profile's best; a board without a track names its
//! event. Every read applies [`crate::visibility::VISIBLE`] with the viewer's game and mode, and
//! takes the viewer's friend and club sets as parameters (empty until SER-M3 steps 7 and 8). Each
//! board read is one statement that also brings each row's player (gamertag and card) and ghost:
//! never one query per row.
//!
//! Writes run on the request's transaction, after its progression fact (the caller's), so a replayed
//! request writes nothing. The ghost's recording is put in the bucket before that transaction
//! ([`ghost_target_in`] says whether it is wanted and already stored; [`save_ghost_in`] records it).
//!
//! The statements composed with `format!` (`sqlx::AssertSqlSafe`) join constants of this crate
//! only ([`VISIBLE`], the board CTEs); every value is a bind parameter.

use crate::gameplane::{Card, Player};
use crate::identity::{IdentityError, ms};
use crate::visibility::{VISIBLE, Viewer};
use fi_domain::UnixMs;
use serde_json::Value as Json;
use sqlx::postgres::PgRow;
use sqlx::types::Json as Jsonb;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The largest ghost recording kept, in bytes (the game asks `DownloadFile` with `maxSize` 256000;
/// real ones are 27 to 72 KB).
pub const MAX_GHOST: usize = 256_000;

/// The largest score accepted (the Beta's bound).
pub const MAX_SCORE: f64 = 1e9;

/// The most a page of a board holds (`maxResults` is clamped to it, as the Beta's).
pub const MAX_PAGE: i64 = 500;

/// Whether a lower score is better: score type 1, a race time in seconds (the Beta's rule; in its
/// captures only type 1 carries fractional values in the tens to hundreds on a track, while types 5
/// and 6, trackless speeds, and 10, trackless points, are higher-is-better).
pub fn lower_is_better(score_type: i32) -> bool {
    score_type == 1
}

/// A score the boards accept: finite, above 0, at most [`MAX_SCORE`].
pub fn valid_score(score: f64) -> bool {
    score.is_finite() && score > 0.0 && score <= MAX_SCORE
}

/// The game's board key of a score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BoardKey {
    /// `scoreboardType` (1 track races, 4 skills, 8 club XP, 9 speed traps and zones, ...).
    pub scoreboard_type: i32,
    /// `type` / `scoreType` (1 time, 5 and 6 speeds, 10 points, ...).
    pub score_type: i32,
    /// `gameScoreboardId`: the event.
    pub game_scoreboard_id: i32,
    /// `trackId`; -1 for boards without a track.
    pub track_id: i32,
}

impl BoardKey {
    /// `scoreboardType:scoreType:gameScoreboardId:trackId`, the event's name in facts.
    pub fn event(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.scoreboard_type, self.score_type, self.game_scoreboard_id, self.track_id
        )
    }
}

/// The assists and the clean flag of a score.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Assists {
    /// `isClean`.
    pub is_clean: bool,
    /// `usedSTM`.
    pub used_stm: bool,
    /// `usedTCS`.
    pub used_tcs: bool,
    /// `usedABS`.
    pub used_abs: bool,
    /// `usedAutoBrake`.
    pub used_auto_brake: bool,
    /// `usedAutoShifting`.
    pub used_auto_shifting: bool,
    /// `usedClutch`.
    pub used_clutch: bool,
}

/// One `Scoreboard.SubmitScore` as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    /// The board.
    pub key: BoardKey,
    /// The score ([`valid_score`]).
    pub score: f64,
    /// `carId`.
    pub car_id: i32,
    /// `carClassId`.
    pub car_class_id: i16,
    /// `carPerformanceIndex`.
    pub car_pi: f32,
    /// Assists and clean flag.
    pub assists: Assists,
    /// The submitted `ScoreboardScoreData` (at most 4 KiB stored).
    pub data: Json,
}

/// The most board keys a profile keeps per mode: a score on a new key beyond it is answered as
/// usual and not stored (the game uses a few hundred events; this bounds what a client can make
/// the table hold).
pub const MAX_BOARDS: i64 = 5_000;

/// Stores `s` as the viewer's best on its board when there is none yet or it is strictly better
/// (an equal score keeps the earlier one); `score_id` is the request's fact id. A new board key
/// beyond [`MAX_BOARDS`] is not stored. Returns whether `s` is now the viewer's best there.
pub async fn submit_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    s: &Submission,
    score_id: Uuid,
) -> Result<bool, IdentityError> {
    let a = s.assists;
    let stored: Option<i32> = sqlx::query_scalar(
        "INSERT INTO game_score (game, mode, profile_id, scoreboard_type, score_type, game_scoreboard_id, track_id, \
           score, score_id, car_id, car_class_id, car_pi, is_clean, used_stm, used_tcs, used_abs, used_auto_brake, \
           used_auto_shifting, used_clutch, data) \
         SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20 \
         WHERE EXISTS (SELECT 1 FROM game_score k WHERE k.game = $1 AND k.mode = $2 AND k.profile_id = $3 \
             AND k.scoreboard_type = $4 AND k.score_type = $5 AND k.game_scoreboard_id = $6 AND k.track_id = $7) \
           OR (SELECT count(*) FROM game_score n WHERE n.game = $1 AND n.mode = $2 AND n.profile_id = $3) < $22 \
         ON CONFLICT (game, mode, profile_id, scoreboard_type, score_type, game_scoreboard_id, track_id) DO UPDATE SET \
           score = EXCLUDED.score, score_id = EXCLUDED.score_id, car_id = EXCLUDED.car_id, \
           car_class_id = EXCLUDED.car_class_id, car_pi = EXCLUDED.car_pi, is_clean = EXCLUDED.is_clean, \
           used_stm = EXCLUDED.used_stm, used_tcs = EXCLUDED.used_tcs, used_abs = EXCLUDED.used_abs, \
           used_auto_brake = EXCLUDED.used_auto_brake, used_auto_shifting = EXCLUDED.used_auto_shifting, \
           used_clutch = EXCLUDED.used_clutch, data = EXCLUDED.data, submitted_at = clock_timestamp() \
         WHERE ($21 AND EXCLUDED.score < game_score.score) OR (NOT $21 AND EXCLUDED.score > game_score.score) \
         RETURNING 1",
    )
    .bind(viewer.game)
    .bind(viewer.mode)
    .bind(viewer.profile_id)
    .bind(s.key.scoreboard_type)
    .bind(s.key.score_type)
    .bind(s.key.game_scoreboard_id)
    .bind(s.key.track_id)
    .bind(s.score)
    .bind(score_id)
    .bind(s.car_id)
    .bind(s.car_class_id)
    .bind(s.car_pi)
    .bind(a.is_clean)
    .bind(a.used_stm)
    .bind(a.used_tcs)
    .bind(a.used_abs)
    .bind(a.used_auto_brake)
    .bind(a.used_auto_shifting)
    .bind(a.used_clutch)
    .bind(Jsonb(&s.data))
    .bind(lower_is_better(s.key.score_type))
    .bind(MAX_BOARDS)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(stored.is_some())
}

/// Whether `score_id` is the viewer's best on its board now (the answer a replayed SubmitScore
/// gives, and the rule UploadFile keeps a ghost by).
pub async fn is_best_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    score_id: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM game_score WHERE game = $1 AND score_id = $2 AND mode = $3 AND profile_id = $4)",
    )
    .bind(viewer.game)
    .bind(score_id)
    .bind(viewer.mode)
    .bind(viewer.profile_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// The viewer's best on exactly `key` (`Scoreboard.GetBestScore`): its score and clean flag.
pub async fn best_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    key: BoardKey,
) -> Result<Option<(f64, bool)>, IdentityError> {
    let row = sqlx::query(
        "SELECT score, is_clean FROM game_score WHERE game = $1 AND mode = $2 AND profile_id = $3 \
           AND scoreboard_type = $4 AND score_type = $5 AND game_scoreboard_id = $6 AND track_id = $7",
    )
    .bind(viewer.game)
    .bind(viewer.mode)
    .bind(viewer.profile_id)
    .bind(key.scoreboard_type)
    .bind(key.score_type)
    .bind(key.game_scoreboard_id)
    .bind(key.track_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match row {
        Some(r) => Some((r.try_get("score")?, r.try_get("is_clean")?)),
        None => None,
    })
}

/// One row of a board as the game shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoreRow {
    /// The real position on the visible board (1-based), whatever view shows it.
    pub position: i64,
    /// The row's profile.
    pub profile_id: Uuid,
    /// The row's player (gamertag parts and card; `tables_tran_id` is not read).
    pub player: Player,
    /// The board key of the row's score (a route board's rows come from any event of the track).
    pub key: BoardKey,
    /// The score.
    pub score: f64,
    /// When it was submitted.
    pub submitted_at: UnixMs,
    /// `carId`.
    pub car_id: i32,
    /// `carClassId`.
    pub car_class_id: i16,
    /// `carPerformanceIndex`.
    pub car_pi: f32,
    /// Assists and clean flag.
    pub assists: Assists,
    /// The score's id (`scoreId`).
    pub score_id: Uuid,
    /// The recording of this very score, if it was uploaded (`ghostFileId`): a newer best without
    /// an upload has none, even when an older score of the key has one.
    pub ghost_file_id: Option<i64>,
}

/// Which rows a board holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoardFilter {
    /// A route board: every event on this track (`trackId`).
    Route(i32),
    /// A board without a track: this event (`pivotId`, the `gameScoreboardId` scores carry).
    Event(i32),
}

impl BoardFilter {
    fn value(self) -> i32 {
        match self {
            BoardFilter::Route(v) | BoardFilter::Event(v) => v,
        }
    }
}

/// A board read (`Scoreboard.GetRows`, `GetRowsAnthem`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoardQuery<'a> {
    /// `scoreboardType`.
    pub scoreboard_type: i32,
    /// `scoreType`.
    pub score_type: i32,
    /// The route or the event.
    pub filter: BoardFilter,
    /// `None`: everyone visible. `Some`: the viewer and these profiles only (friends, club), each
    /// row keeping its real position.
    pub circle: Option<&'a [Uuid]>,
    /// The page's first row among the rows shown (1-based), unless [`BoardQuery::near_me`] and the
    /// viewer has a row.
    pub start: i64,
    /// The page's length (0 to [`MAX_PAGE`]).
    pub max: i64,
    /// The page with the viewer in its middle (view "near me").
    pub near_me: bool,
}

/// A page of a board.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardPage {
    /// The page, in board order.
    pub rows: Vec<ScoreRow>,
    /// The viewer's row, if they have a score on the board.
    pub me: Option<ScoreRow>,
    /// How many rows the view shows in all (`available`).
    pub available: i64,
}

/// What a ranking reads: one board (the game's `GetRows`), or every board of one family at once,
/// each ranked apart (the play dashboard's index, `crate::dashboard::boards`). The bind layout is
/// fixed: `$1` game, `$2` mode, `$3` scoreboard type, `$4` score type (an `integer[]` for
/// [`Ranking::EachScoreType`]), `$5` the track or event (an `integer[]` for
/// [`Ranking::EachTrack`] and [`Ranking::EachEvent`]), `$6` lower is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ranking {
    /// One board: `$4` the score type, `$5` the route's track or the event.
    Board(BoardFilter),
    /// Every route of `$5` (tracks), ranked per track.
    EachTrack,
    /// Every event of `$5` (events), ranked per event.
    EachEvent,
    /// Every score type of `$4` on event `$5` (the game's single trackless boards), ranked per
    /// score type.
    EachScoreType,
}

impl Ranking {
    /// The `game_score` column that names a row's board within the ranking (`ranked.<column>`).
    pub fn board_column(self) -> &'static str {
        match self {
            Ranking::Board(BoardFilter::Route(_)) | Ranking::EachTrack => "track_id",
            Ranking::Board(BoardFilter::Event(_)) | Ranking::EachEvent => "game_scoreboard_id",
            Ranking::EachScoreType => "score_type",
        }
    }
}

/// Each visible profile's best on a board, best first: the statement's `best` and `ranked` CTEs
/// ([`Ranking`] gives the bind layout). One board's statement texts are each a plain equality: a
/// generic plan (sqlx keeps prepared statements, and Postgres switches to one after five runs)
/// still reads only the board's rows through `game_score_track` or `game_score_event`, which an
/// `$n IS NULL OR` filter would not. `player` is the profile and account columns `best` carries
/// besides the score's (a constant of this crate: the game's reads take the XUID and name, the
/// dashboard's never an XUID); the order, the deduplication and the visibility are this one text
/// for every reader, so a rank anywhere equals the rank in game.
pub(crate) fn ranking_ctes(ranking: Ranking, player: &'static str) -> String {
    let (on, group) = match ranking {
        Ranking::Board(BoardFilter::Route(_)) => ("s.score_type = $4 AND s.track_id = $5", None),
        Ranking::Board(BoardFilter::Event(_)) => {
            ("s.score_type = $4 AND s.game_scoreboard_id = $5", None)
        }
        Ranking::EachTrack => (
            "s.score_type = $4 AND s.track_id = ANY($5::integer[])",
            Some("track_id"),
        ),
        Ranking::EachEvent => (
            "s.score_type = $4 AND s.game_scoreboard_id = ANY($5::integer[])",
            Some("game_scoreboard_id"),
        ),
        Ranking::EachScoreType => (
            "s.score_type = ANY($4::integer[]) AND s.game_scoreboard_id = $5",
            Some("score_type"),
        ),
    };
    let (distinct, partition) = match group {
        Some(c) => (
            format!("s.{c}, s.profile_id"),
            format!("PARTITION BY b.{c} "),
        ),
        None => ("s.profile_id".to_owned(), String::new()),
    };
    format!(
        "best AS ( \
           SELECT DISTINCT ON ({distinct}) s.profile_id, s.scoreboard_type, s.score_type, s.game_scoreboard_id, \
             s.track_id, s.score, s.score_id, s.submitted_at, s.car_id, s.car_class_id, s.car_pi, s.is_clean, \
             s.used_stm, s.used_tcs, s.used_abs, s.used_auto_brake, s.used_auto_shifting, s.used_clutch, \
             {player} \
           FROM game_score s \
           JOIN profile p ON p.id = s.profile_id \
           JOIN account a ON a.id = p.account_id \
           WHERE s.game = $1 AND s.mode = $2 AND s.scoreboard_type = $3 AND {on} \
             AND p.mode = s.mode AND {VISIBLE} \
           ORDER BY {distinct}, CASE WHEN $6 THEN s.score ELSE -s.score END, s.submitted_at \
         ), \
         ranked AS ( \
           SELECT b.*, row_number() OVER ({partition}ORDER BY CASE WHEN $6 THEN b.score ELSE -b.score END, b.submitted_at, b.profile_id) AS position \
           FROM best b \
         )"
    )
}

/// The game's own reads' ranking ([`ranking_ctes`] of one board, with the XUID and name).
fn best_cte(filter: BoardFilter) -> String {
    ranking_ctes(Ranking::Board(filter), "p.xuid, p.name")
}

/// The columns of a row with its card and ghost (`r` a `ranked` row, `l` its logon, `g` its ghost).
const ROW_COLUMNS: &str = "r.profile_id, r.position, r.scoreboard_type, r.score_type, r.game_scoreboard_id, \
     r.track_id, r.score, r.score_id, r.submitted_at, r.car_id, r.car_class_id, r.car_pi, r.is_clean, r.used_stm, \
     r.used_tcs, r.used_abs, r.used_auto_brake, r.used_auto_shifting, r.used_clutch, r.xuid, r.name, \
     l.career_level, l.badge_id, l.player_title_id, g.file_id";

/// The joins of [`ROW_COLUMNS`] (`$1` game): the logon's card, and the ghost of this very score.
const ROW_JOINS: &str = "LEFT JOIN game_logon l ON l.game = $1 AND l.profile_id = r.profile_id \
     LEFT JOIN game_ghost g ON g.game = $1 AND g.score_id = r.score_id";

fn score_row(r: &PgRow) -> Result<ScoreRow, sqlx::Error> {
    let level: Option<i64> = r.try_get("career_level")?;
    let badge: Option<i32> = r.try_get("badge_id")?;
    let title: Option<i32> = r.try_get("player_title_id")?;
    let submitted: OffsetDateTime = r.try_get("submitted_at")?;
    Ok(ScoreRow {
        position: r.try_get("position")?,
        profile_id: r.try_get("profile_id")?,
        player: Player {
            xuid: r.try_get("xuid")?,
            name: r.try_get("name")?,
            card: Card {
                career_level: level.and_then(|v| u32::try_from(v).ok()).unwrap_or(0),
                badge_id: badge.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
                player_title_id: title.and_then(|v| u16::try_from(v).ok()).unwrap_or(0),
            },
            tables_tran_id: None,
        },
        key: BoardKey {
            scoreboard_type: r.try_get("scoreboard_type")?,
            score_type: r.try_get("score_type")?,
            game_scoreboard_id: r.try_get("game_scoreboard_id")?,
            track_id: r.try_get("track_id")?,
        },
        score: r.try_get("score")?,
        submitted_at: ms(submitted),
        car_id: r.try_get("car_id")?,
        car_class_id: r.try_get("car_class_id")?,
        car_pi: r.try_get("car_pi")?,
        assists: Assists {
            is_clean: r.try_get("is_clean")?,
            used_stm: r.try_get("used_stm")?,
            used_tcs: r.try_get("used_tcs")?,
            used_abs: r.try_get("used_abs")?,
            used_auto_brake: r.try_get("used_auto_brake")?,
            used_auto_shifting: r.try_get("used_auto_shifting")?,
            used_clutch: r.try_get("used_clutch")?,
        },
        score_id: r.try_get("score_id")?,
        ghost_file_id: r.try_get("file_id")?,
    })
}

/// A page of a board for the viewer, with the viewer's own row and the view's row count (the
/// Beta's `score_rows`): positions are the real ones on the visible board; a circle keeps only the
/// viewer and its members; near me is the page with the viewer in its middle (`start` when the
/// viewer has no row). One statement.
pub async fn board_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    q: &BoardQuery<'_>,
) -> Result<BoardPage, IdentityError> {
    let sql = format!(
        "WITH {best}, \
         shown AS ( \
           SELECT r.*, row_number() OVER (ORDER BY r.position) AS idx FROM ranked r \
           WHERE $7::uuid[] IS NULL OR r.profile_id = $8 OR r.profile_id = ANY($7::uuid[]) \
         ), \
         total AS (SELECT count(*) AS available FROM shown), \
         first AS ( \
           SELECT COALESCE(CASE WHEN $9 THEN (SELECT GREATEST(1, idx - $11 / 2) FROM shown WHERE profile_id = $8) END, \
             GREATEST($10, 1)) AS f \
         ) \
         SELECT t.available, x.* FROM total t \
         LEFT JOIN LATERAL ( \
           SELECT {ROW_COLUMNS}, r.idx, \
             (r.idx >= (SELECT f FROM first) AND r.idx < (SELECT f FROM first) + $11) AS on_page \
           FROM shown r {ROW_JOINS} \
           WHERE (r.idx >= (SELECT f FROM first) AND r.idx < (SELECT f FROM first) + $11) OR r.profile_id = $8 \
         ) x ON true \
         ORDER BY x.idx",
        best = best_cte(q.filter),
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(q.scoreboard_type)
        .bind(q.score_type)
        .bind(q.filter.value())
        .bind(lower_is_better(q.score_type))
        .bind(q.circle)
        .bind(viewer.profile_id)
        .bind(q.near_me)
        .bind(q.start)
        .bind(q.max.clamp(0, MAX_PAGE))
        .fetch_all(&mut *conn)
        .await?;
    let mut page = BoardPage::default();
    for r in &rows {
        page.available = r.try_get("available")?;
        // No row of the board on the page and none of the viewer's: the LEFT JOIN's NULLs.
        let Some(on_page) = r.try_get::<Option<bool>, _>("on_page")? else {
            continue;
        };
        let row = score_row(r)?;
        if row.profile_id == viewer.profile_id {
            page.me = Some(row.clone());
        }
        if on_page {
            page.rows.push(row);
        }
    }
    Ok(page)
}

/// A rival pick on a route (`Scoreboard.GetRival`; the Beta's `pick_rival` and `challenge_rival`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RivalQuery<'a> {
    /// `scoreboardType`.
    pub scoreboard_type: i32,
    /// `scoreType`.
    pub score_type: i32,
    /// The route: every event on this track.
    pub track: i32,
    /// One event's board instead of the route (a Monthly Rivals event: the scores'
    /// `gameScoreboardId`).
    pub event: Option<i32>,
    /// Only rows in one of these cars (the car class the game asks for); `None`: any car.
    pub cars: Option<&'a [i32]>,
    /// The viewer's friends, tried first (empty until SER-M3 step 7).
    pub friends: &'a [Uuid],
    /// Exactly this player (`rivalXuid`); `None`: the closest to the viewer's best.
    pub xuid: Option<i64>,
}

/// Of `tracks`, those with at least one time (score type 1) on one of `boards` by a visible
/// profile of the viewer's game and mode other than the viewer: the tracks a rival can be found
/// on from real data (Monthly Rivals lists only those).
pub async fn tracks_with_rivals_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    boards: &[i32],
    tracks: &[i32],
) -> Result<Vec<i32>, IdentityError> {
    if tracks.is_empty() || boards.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT DISTINCT s.track_id FROM game_score s \
         JOIN profile p ON p.id = s.profile_id \
         JOIN account a ON a.id = p.account_id \
         WHERE s.game = $1 AND s.mode = $2 AND s.score_type = 1 \
           AND s.scoreboard_type = ANY($4::integer[]) AND s.track_id = ANY($5::integer[]) \
           AND s.profile_id <> $3 AND p.game = $1 AND p.mode = $2 AND {VISIBLE}"
    );
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(viewer.profile_id)
        .bind(boards)
        .bind(tracks)
        .fetch_all(&mut *conn)
        .await?)
}

/// The Monthly Rivals boards' `scoreboardType` (S2; other projects used as reference): their scores'
/// `gameScoreboardId` is the event id.
pub const MONTHLY_BOARD: i32 = 3;

/// The viewer's rival on a route (or on one event's board, `q.event`): another visible profile's
/// best there, a friend's first, the one
/// closest to the viewer's own best (the best on the route when the viewer has none), or exactly
/// `q.xuid`'s. `None` when nobody else qualifies.
pub async fn rival_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    q: &RivalQuery<'_>,
) -> Result<Option<ScoreRow>, IdentityError> {
    rival_row_in(conn, viewer, q, false).await
}

/// The viewer's own best on the route (or on `q.event`'s board) as a rival row (the game races a
/// player against their own ghost: IsRivalSelf), in one of `q.cars` when given; `None` when the
/// viewer has no visible time there.
pub async fn own_rival_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    q: &RivalQuery<'_>,
) -> Result<Option<ScoreRow>, IdentityError> {
    rival_row_in(conn, viewer, q, true).await
}

async fn rival_row_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    q: &RivalQuery<'_>,
    own: bool,
) -> Result<Option<ScoreRow>, IdentityError> {
    let filter = q
        .event
        .map_or(BoardFilter::Route(q.track), BoardFilter::Event);
    let whose = if own {
        "r.profile_id = $7"
    } else {
        "r.profile_id <> $7"
    };
    let sql = format!(
        "WITH {best}, \
         mine AS (SELECT score FROM ranked WHERE profile_id = $7) \
         SELECT {ROW_COLUMNS} FROM ranked r {ROW_JOINS} \
         WHERE {whose} AND ($8::integer[] IS NULL OR r.car_id = ANY($8::integer[])) \
           AND ($10::bigint IS NULL OR r.xuid = $10) \
         ORDER BY (r.profile_id = ANY($9::uuid[])) DESC, \
           abs(r.score - COALESCE((SELECT score FROM mine), r.score)), r.position \
         LIMIT 1",
        best = best_cte(filter),
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(q.scoreboard_type)
        .bind(q.score_type)
        .bind(filter.value())
        .bind(lower_is_better(q.score_type))
        .bind(viewer.profile_id)
        .bind(q.cars)
        .bind(q.friends)
        .bind(q.xuid)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.as_ref().map(score_row).transpose()?)
}

/// The visible ghost on a route closest to `score` (any event of the track, or only `event`'s,
/// anyone's, the viewer's included): a rival without a ghost of their own is raced against it (the Beta's
/// `nearest_ghost`).
pub async fn nearest_ghost_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    scoreboard_type: i32,
    score_type: i32,
    track: i32,
    event: Option<i32>,
    score: f64,
) -> Result<Option<i64>, IdentityError> {
    let sql = format!(
        "SELECT g.file_id FROM game_ghost g \
         JOIN profile p ON p.id = g.profile_id \
         JOIN account a ON a.id = p.account_id \
         WHERE g.game = $1 AND g.mode = $2 AND g.scoreboard_type = $3 AND g.score_type = $4 AND g.track_id = $5 \
           AND ($7::integer IS NULL OR g.game_scoreboard_id = $7) \
           AND p.mode = g.mode AND {VISIBLE} \
         ORDER BY abs(g.score - $6), g.file_id LIMIT 1"
    );
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(viewer.mode)
        .bind(scoreboard_type)
        .bind(score_type)
        .bind(track)
        .bind(score)
        .bind(event)
        .fetch_optional(&mut *conn)
        .await?)
}

/// What an UploadFile names: the viewer's current best with that scoreId.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GhostTarget {
    /// Its board.
    pub key: BoardKey,
    /// Its score.
    pub score: f64,
    /// Whether a recording with the upload's digest is already recorded (`ghost_blob`): the bucket
    /// already holds it.
    pub blob_known: bool,
}

/// The viewer's best with `score_id` (`None`: not the viewer's, or no longer their best: the
/// ghost is discarded), and whether `sha256` is already recorded. Read before the recording is put.
pub async fn ghost_target_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    score_id: Uuid,
    sha256: &[u8; 32],
) -> Result<Option<GhostTarget>, IdentityError> {
    let row = sqlx::query(
        "SELECT s.scoreboard_type, s.score_type, s.game_scoreboard_id, s.track_id, s.score, \
           EXISTS (SELECT 1 FROM ghost_blob b WHERE b.sha256 = $5) AS blob_known \
         FROM game_score s WHERE s.game = $1 AND s.score_id = $2 AND s.mode = $3 AND s.profile_id = $4",
    )
    .bind(viewer.game)
    .bind(score_id)
    .bind(viewer.mode)
    .bind(viewer.profile_id)
    .bind(sha256.as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(GhostTarget {
        key: BoardKey {
            scoreboard_type: r.try_get("scoreboard_type")?,
            score_type: r.try_get("score_type")?,
            game_scoreboard_id: r.try_get("game_scoreboard_id")?,
            track_id: r.try_get("track_id")?,
        },
        score: r.try_get("score")?,
        blob_known: r.try_get("blob_known")?,
    }))
}

/// A ghost as recorded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SavedGhost {
    /// `ghostFileId` (kept when a ghost of the same board replaces it).
    pub file_id: i64,
    /// Its board.
    pub key: BoardKey,
    /// The score it recorded.
    pub score: f64,
}

/// UploadFile, after the recording was put in the bucket: records that it exists (`ghost_blob`)
/// and makes it the viewer's ghost on the board of `score_id`, when that score is still the
/// viewer's best (the score row is locked until the transaction ends, so a better score submitted
/// meanwhile waits). `None`: no longer the best, nothing written. `event_id` is the request's fact.
pub async fn save_ghost_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    score_id: Uuid,
    sha256: &[u8; 32],
    size: i32,
    event_id: Uuid,
) -> Result<Option<SavedGhost>, IdentityError> {
    let target: Option<(i32, i32, i32, i32, f64, i32)> = sqlx::query_as(
        "SELECT scoreboard_type, score_type, game_scoreboard_id, track_id, score, car_id FROM game_score \
         WHERE game = $1 AND score_id = $2 AND mode = $3 AND profile_id = $4 FOR SHARE",
    )
    .bind(viewer.game)
    .bind(score_id)
    .bind(viewer.mode)
    .bind(viewer.profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((scoreboard_type, score_type, game_scoreboard_id, track_id, score, car_id)) = target
    else {
        return Ok(None);
    };
    sqlx::query(
        "INSERT INTO ghost_blob (sha256, game, size, uploaded_by) VALUES ($1, $2, $3, $4) ON CONFLICT (sha256) DO NOTHING",
    )
    .bind(sha256.as_slice())
    .bind(viewer.game)
    .bind(size)
    .bind(viewer.profile_id)
    .execute(&mut *conn)
    .await?;
    let file_id: i64 = sqlx::query_scalar(
        "INSERT INTO game_ghost (game, mode, profile_id, scoreboard_type, score_type, game_scoreboard_id, track_id, \
           score_id, score, car_id, blob_sha256, size, event_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT (game, mode, profile_id, scoreboard_type, score_type, game_scoreboard_id, track_id) DO UPDATE SET \
           score_id = EXCLUDED.score_id, score = EXCLUDED.score, car_id = EXCLUDED.car_id, \
           blob_sha256 = EXCLUDED.blob_sha256, size = EXCLUDED.size, event_id = EXCLUDED.event_id, \
           recorded_at = clock_timestamp() \
         RETURNING file_id",
    )
    .bind(viewer.game)
    .bind(viewer.mode)
    .bind(viewer.profile_id)
    .bind(scoreboard_type)
    .bind(score_type)
    .bind(game_scoreboard_id)
    .bind(track_id)
    .bind(score_id)
    .bind(score)
    .bind(car_id)
    .bind(sha256.as_slice())
    .bind(size)
    .bind(event_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(SavedGhost {
        file_id,
        key: BoardKey {
            scoreboard_type,
            score_type,
            game_scoreboard_id,
            track_id,
        },
        score,
    }))
}

/// DownloadFile: the recording's digest and size of ghost `file_id`, when its owner is visible to
/// the viewer (same game and mode, active, not banned).
pub async fn ghost_blob_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    file_id: i64,
) -> Result<Option<([u8; 32], i32)>, IdentityError> {
    let sql = format!(
        "SELECT g.blob_sha256, g.size FROM game_ghost g \
         JOIN profile p ON p.id = g.profile_id \
         JOIN account a ON a.id = p.account_id \
         WHERE g.game = $1 AND g.file_id = $2 AND g.mode = $3 AND p.mode = g.mode AND {VISIBLE}"
    );
    let row: Option<(Vec<u8>, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(viewer.game)
        .bind(file_id)
        .bind(viewer.mode)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.and_then(|(sha, size)| <[u8; 32]>::try_from(sha.as_slice()).ok().map(|d| (d, size))))
}

/// XP the viewer's profile reported in the last hour (database clock): the XP-rate gate's input.
pub async fn xp_last_hour_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
) -> Result<f64, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(sum(xp), 0)::double precision FROM game_xp \
         WHERE game = $1 AND profile_id = $2 AND at >= clock_timestamp() - interval '1 hour'",
    )
    .bind(viewer.game)
    .bind(viewer.profile_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Adds an accepted XP report (`Scoreboard.SubmitXpScore`, already clamped) to the ledger under
/// its fact's id, and to the profile's club membership of the viewer's mode (the club ladder and
/// club XP, `crate::clubs`); a second call with the same id writes nothing.
pub async fn add_xp_in(
    conn: &mut PgConnection,
    viewer: &Viewer<'_>,
    event_id: Uuid,
    xp: f64,
) -> Result<(), IdentityError> {
    let done = sqlx::query(
        "INSERT INTO game_xp (event_id, game, mode, profile_id, xp) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (event_id) DO NOTHING",
    )
    .bind(event_id)
    .bind(viewer.game)
    .bind(viewer.mode)
    .bind(viewer.profile_id)
    .bind(xp)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 1 {
        crate::clubs::add_member_xp_in(conn, viewer.game, viewer.mode, viewer.profile_id, xp)
            .await?;
    }
    Ok(())
}
