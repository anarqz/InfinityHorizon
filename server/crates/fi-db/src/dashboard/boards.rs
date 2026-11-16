//! The play dashboard's leaderboards (DASHBOARD_API.md §3, PD-2; served by
//! `fi_surfaces::dashboard`): the game's own boards, read with the game's own ranking.
//!
//! **Ranking.** Every statement here ranks with [`scores::ranking_ctes`], the very text behind
//! `Scoreboard.GetRows` ([`scores::board_in`]): the same visibility ([`crate::visibility::VISIBLE`],
//! the row's mode its profile's), each profile's best row, the same order and ties. So a rank here
//! is the rank in game.
//!
//! **Website listing, after ranking.** A row whose profile is hidden from the website
//! (`web_listed = false` or `social_hidden_at` set) is omitted after the ranking and the others keep
//! their game rank (gaps allowed, never re-ranked). The `class` filter is the same kind of
//! after-ranking filter, on the row's `car_class_id` (the game's GetRows has no class).
//!
//! **What is read.** The plain name, the Discord display name and whether an avatar exists (as
//! `GET /lobbies`' host card), the club tag of the board's mode (never a private club's,
//! [`crate::clubs::PRIVACY_PRIVATE`]). No statement here selects an XUID, an account id or a Discord
//! user id (DASHBOARD_API.md §1).

use crate::identity::{IdentityError, ms};
use crate::scores::{self, Assists, BoardFilter, Ranking};
use fi_domain::UnixMs;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;

/// The profile and account columns the dashboard's rankings carry (no XUID, no ids): the name, the
/// website listing, the Discord display name and the avatar flag (as `GET /lobbies`' host card).
const PLAYER: &str = "p.name, (p.social_hidden_at IS NULL AND p.web_listed) AS web_listed, \
     CASE WHEN a.discord_user_id IS NOT NULL THEN coalesce(a.discord_global_name, a.discord_username) END AS discord_name, \
     (a.discord_avatar IS NOT NULL) AS avatar";

/// Exact current-best ghost references for the private offline Explore processor.
/// Ranking and listing are identical to route boards; raw bytes are never read here.
#[derive(Clone, Debug)]
pub struct ReplaySource {
    /// Private identity used only to derive the snapshot ref.
    pub profile_id: uuid::Uuid,
    /// Current listed name.
    pub name: String,
    /// Native track id.
    pub track_id: i32,
    /// The exact score owning the recording.
    pub score: f64,
    /// Immutable blob SHA-256, lowercase hex.
    pub sha256: String,
    /// Compressed blob length.
    pub size: i32,
}

/// Returns all listed current-best route recordings using the game's ranking.
pub async fn replay_sources_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    scoreboard_type: i32,
    tracks: &[i32],
) -> Result<Vec<ReplaySource>, IdentityError> {
    let family = Family::Tracks {
        scoreboard_type,
        score_type: 1,
        tracks,
    };
    let sql = format!(
        "WITH {} SELECT r.profile_id, r.name, r.track_id, r.score, \
         encode(g.blob_sha256, 'hex') AS sha256, g.size FROM ranked r \
         JOIN game_ghost g ON g.game=$1 AND g.mode=$2 AND g.profile_id=r.profile_id \
           AND g.score_id=r.score_id WHERE r.web_listed ORDER BY r.track_id, r.position",
        scores::ranking_ctes(scores::Ranking::EachTrack, PLAYER),
    );
    let rows = family
        .bind(sqlx::query(sqlx::AssertSqlSafe(sql)), game, mode)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(ReplaySource {
                profile_id: r.try_get("profile_id")?,
                name: r.try_get("name")?,
                track_id: r.try_get("track_id")?,
                score: r.try_get("score")?,
                sha256: r.try_get("sha256")?,
                size: r.try_get("size")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// The largest page of [`board_rows_in`].
pub const MAX_ROWS: i64 = 100;

/// Every board of one family, for the index: one grouped statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family<'a> {
    /// Route boards of `tracks` (rivals 1/1, Showcases 6/1).
    Tracks {
        /// `scoreboardType`.
        scoreboard_type: i32,
        /// `scoreType`.
        score_type: i32,
        /// The tracks.
        tracks: &'a [i32],
    },
    /// Event boards of `events` (speed cameras 9/5 and 9/6, Monthly Rivals 3/1, the XP board 8/4).
    Events {
        /// `scoreboardType`.
        scoreboard_type: i32,
        /// `scoreType`.
        score_type: i32,
        /// The events (`gameScoreboardId`).
        events: &'a [i32],
    },
    /// The single trackless boards of `score_types` on `event` (the ambient stats 4/7-4/15).
    ScoreTypes {
        /// `scoreboardType`.
        scoreboard_type: i32,
        /// The score types; all rank the same way.
        score_types: &'a [i32],
        /// `gameScoreboardId` (0).
        event: i32,
    },
}

/// One board of the index.
#[derive(Clone, Debug, PartialEq)]
pub struct BoardStat {
    /// The track, event or score type.
    pub board: i32,
    /// Rows shown (website-listed).
    pub entries: i64,
    /// The #1's name and score, when the #1 is website-listed.
    pub top: Option<(String, f64)>,
}

/// The index of one family: each board that has a visible row (absent boards have none). One
/// statement, whatever the number of boards.
pub async fn family_stats_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    family: &Family<'_>,
) -> Result<Vec<BoardStat>, IdentityError> {
    let (ranking, _, _) = family.ranking();
    let column = ranking.board_column();
    let sql = format!(
        "WITH {ctes} \
         SELECT r.{column} AS board, count(*) FILTER (WHERE r.web_listed) AS entries, \
           max(r.name) FILTER (WHERE r.position = 1 AND r.web_listed) AS top_name, \
           max(r.score) FILTER (WHERE r.position = 1 AND r.web_listed) AS top_score \
         FROM ranked r GROUP BY r.{column} ORDER BY r.{column}",
        ctes = scores::ranking_ctes(ranking, PLAYER),
    );
    let rows = family
        .bind(sqlx::query(sqlx::AssertSqlSafe(sql)), game, mode)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            let name: Option<String> = r.try_get("top_name")?;
            let score: Option<f64> = r.try_get("top_score")?;
            Ok(BoardStat {
                board: r.try_get("board")?,
                entries: r.try_get("entries")?,
                top: name.zip(score),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

impl Family<'_> {
    /// The grouped ranking, the scoreboard type and whether lower is better.
    fn ranking(&self) -> (Ranking, i32, bool) {
        match self {
            Family::Tracks {
                scoreboard_type,
                score_type,
                ..
            } => (
                Ranking::EachTrack,
                *scoreboard_type,
                scores::lower_is_better(*score_type),
            ),
            Family::Events {
                scoreboard_type,
                score_type,
                ..
            } => (
                Ranking::EachEvent,
                *scoreboard_type,
                scores::lower_is_better(*score_type),
            ),
            Family::ScoreTypes {
                scoreboard_type,
                score_types,
                ..
            } => {
                let lower = score_types
                    .first()
                    .is_some_and(|t| scores::lower_is_better(*t));
                debug_assert!(
                    score_types
                        .iter()
                        .all(|t| scores::lower_is_better(*t) == lower)
                );
                (Ranking::EachScoreType, *scoreboard_type, lower)
            }
        }
    }

    /// Binds `$1` to `$6` of [`scores::ranking_ctes`]' grouped layout.
    fn bind<'q>(
        &'q self,
        q: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
        game: &'q str,
        mode: &'q str,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        let (_, sbt, lower) = self.ranking();
        let q = q.bind(game).bind(mode).bind(sbt);
        let q = match self {
            Family::Tracks {
                score_type, tracks, ..
            } => q.bind(*score_type).bind(*tracks),
            Family::Events {
                score_type, events, ..
            } => q.bind(*score_type).bind(*events),
            Family::ScoreTypes {
                score_types, event, ..
            } => q.bind(*score_types).bind(*event),
        };
        q.bind(lower)
    }
}

/// A profile's row on one board of a family, at its game rank.
#[derive(Clone, Debug, PartialEq)]
pub struct ProfileRank {
    /// The track, event or score type.
    pub board: i32,
    /// The game's rank.
    pub rank: i64,
    /// The score as stored.
    pub score: f64,
    /// When it was submitted.
    pub submitted_at: UnixMs,
    /// `carId`.
    pub car_id: i32,
    /// `carClassId`.
    pub car_class_id: i16,
    /// `carPerformanceIndex`.
    pub car_pi: f32,
}

/// `profile_id`'s game rank on each board of `family` it has a row on: the family's grouped
/// ranking ([`scores::ranking_ctes`], so ranks equal the game's), kept to the profile's rows. The
/// caller passes only the boards the profile has rows on ([`profile_boards_in`]), so only those
/// boards are ranked. One statement.
pub async fn profile_ranks_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    family: &Family<'_>,
    profile_id: uuid::Uuid,
) -> Result<Vec<ProfileRank>, IdentityError> {
    let (ranking, _, _) = family.ranking();
    let column = ranking.board_column();
    let sql = format!(
        "WITH {ctes} \
         SELECT r.{column} AS board, r.position, r.score, r.submitted_at, r.car_id, r.car_class_id, r.car_pi \
         FROM ranked r WHERE r.profile_id = $7 ORDER BY r.{column}",
        ctes = scores::ranking_ctes(ranking, PLAYER),
    );
    let rows = family
        .bind(sqlx::query(sqlx::AssertSqlSafe(sql)), game, mode)
        .bind(profile_id)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(ProfileRank {
                board: r.try_get("board")?,
                rank: r.try_get("position")?,
                score: r.try_get("score")?,
                submitted_at: ms(r.try_get::<OffsetDateTime, _>("submitted_at")?),
                car_id: r.try_get("car_id")?,
                car_class_id: r.try_get("car_class_id")?,
                car_pi: r.try_get("car_pi")?,
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// The board keys of every row `profile_id` holds in `game` and `mode` (at most
/// `scores::MAX_BOARDS`): `(scoreboardType, scoreType, gameScoreboardId, trackId)`. One statement.
pub async fn profile_boards_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    profile_id: uuid::Uuid,
) -> Result<Vec<scores::BoardKey>, IdentityError> {
    let rows: Vec<(i32, i32, i32, i32)> = sqlx::query_as(
        "SELECT scoreboard_type, score_type, game_scoreboard_id, track_id FROM game_score \
         WHERE game = $1 AND mode = $2 AND profile_id = $3",
    )
    .bind(game)
    .bind(mode)
    .bind(profile_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(scoreboard_type, score_type, game_scoreboard_id, track_id)| scores::BoardKey {
                scoreboard_type,
                score_type,
                game_scoreboard_id,
                track_id,
            },
        )
        .collect())
}

/// One board, as the game's GetRows names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OneBoard {
    /// `scoreboardType`.
    pub scoreboard_type: i32,
    /// `scoreType`.
    pub score_type: i32,
    /// The route or the event.
    pub filter: BoardFilter,
}

impl OneBoard {
    fn ctes(&self) -> String {
        scores::ranking_ctes(Ranking::Board(self.filter), PLAYER)
    }

    fn value(&self) -> i32 {
        match self.filter {
            BoardFilter::Route(v) | BoardFilter::Event(v) => v,
        }
    }
}

/// A board's counts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BoardCounts {
    /// Rows shown (website-listed), every class.
    pub entries: i64,
    /// The #1's name and score, when the #1 is website-listed.
    pub top: Option<(String, f64)>,
    /// Rows shown per `car_class_id`, by class id.
    pub classes: Vec<(i16, i64)>,
}

/// A board's counts: one statement.
pub async fn board_counts_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    b: &OneBoard,
) -> Result<BoardCounts, IdentityError> {
    let sql = format!(
        "WITH {ctes} \
         SELECT r.car_class_id, count(*) FILTER (WHERE r.web_listed) AS entries, \
           max(r.name) FILTER (WHERE r.position = 1 AND r.web_listed) AS top_name, \
           max(r.score) FILTER (WHERE r.position = 1 AND r.web_listed) AS top_score \
         FROM ranked r GROUP BY r.car_class_id ORDER BY r.car_class_id",
        ctes = b.ctes(),
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(mode)
        .bind(b.scoreboard_type)
        .bind(b.score_type)
        .bind(b.value())
        .bind(scores::lower_is_better(b.score_type))
        .fetch_all(&mut *conn)
        .await?;
    let mut out = BoardCounts::default();
    for r in &rows {
        let class: i16 = r.try_get("car_class_id")?;
        let entries: i64 = r.try_get("entries")?;
        out.entries += entries;
        if entries > 0 {
            out.classes.push((class, entries));
        }
        let name: Option<String> = r.try_get("top_name")?;
        let score: Option<f64> = r.try_get("top_score")?;
        if let Some(top) = name.zip(score) {
            out.top = Some(top);
        }
    }
    Ok(out)
}

/// One row of a board as the dashboard shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct WebRow {
    /// The game's rank.
    pub rank: i64,
    /// The row's profile (server-side only: the snapshot's profiles; never on the wire).
    pub profile_id: uuid::Uuid,
    /// The plain name.
    pub name: String,
    /// The linked Discord account's global name, else its username.
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
    /// The club tag of the board's mode, unless the club is private.
    pub club_tag: Option<String>,
    /// The score as stored.
    pub score: f64,
    /// When it was submitted.
    pub submitted_at: UnixMs,
    /// `carId`.
    pub car_id: i32,
    /// `carClassId`.
    pub car_class_id: i16,
    /// `carPerformanceIndex` (0.608).
    pub car_pi: f32,
    /// Assists and clean flag.
    pub assists: Assists,
}

/// A page of a board: the website-listed rows (of `class` when given) in rank order, skipping
/// `offset`, at most `limit` (clamped to 0..=[`MAX_ROWS`]), and how many there are in all. One
/// statement.
pub async fn board_rows_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    b: &OneBoard,
    class: Option<i16>,
    offset: i64,
    limit: i64,
) -> Result<(i64, Vec<WebRow>), IdentityError> {
    let sql = format!(
        "WITH {ctes}, \
         shown AS ( \
           SELECT r.*, row_number() OVER (ORDER BY r.position) AS idx FROM ranked r \
           WHERE r.web_listed AND ($7::smallint IS NULL OR r.car_class_id = $7) \
         ), \
         total AS (SELECT count(*) AS total FROM shown) \
         SELECT t.total, x.* FROM total t \
         LEFT JOIN LATERAL ( \
           SELECT r.idx, r.position, r.profile_id, r.name, r.discord_name, r.avatar, r.score, r.submitted_at, r.car_id, \
             r.car_class_id, r.car_pi, r.is_clean, r.used_stm, r.used_tcs, r.used_abs, r.used_auto_brake, \
             r.used_auto_shifting, r.used_clutch, c.tag AS club_tag \
           FROM shown r \
           LEFT JOIN game_club_member m ON m.game = $1 AND m.profile_id = r.profile_id \
           LEFT JOIN game_club c ON c.game = $1 AND c.id = m.club_id AND c.mode = $2 \
             AND c.privacy <> {private} \
           WHERE r.idx > $8 AND r.idx <= $8 + $9 \
         ) x ON true \
         ORDER BY x.idx",
        ctes = b.ctes(),
        private = crate::clubs::PRIVACY_PRIVATE,
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(mode)
        .bind(b.scoreboard_type)
        .bind(b.score_type)
        .bind(b.value())
        .bind(scores::lower_is_better(b.score_type))
        .bind(class)
        .bind(offset.max(0))
        .bind(limit.clamp(0, MAX_ROWS))
        .fetch_all(&mut *conn)
        .await?;
    let mut total = 0;
    let mut out = Vec::new();
    for r in &rows {
        total = r.try_get("total")?;
        // No row on the page: the LEFT JOIN's NULLs.
        let Some(rank) = r.try_get::<Option<i64>, _>("position")? else {
            continue;
        };
        out.push(web_row(r, rank)?);
    }
    Ok((total, out))
}

/// A [`WebRow`] of a statement selecting the row columns (`position` already read as `rank`).
fn web_row(r: &sqlx::postgres::PgRow, rank: i64) -> Result<WebRow, sqlx::Error> {
    Ok(WebRow {
        rank,
        profile_id: r.try_get("profile_id")?,
        name: r.try_get("name")?,
        discord_name: r.try_get("discord_name")?,
        avatar: r.try_get("avatar")?,
        club_tag: r.try_get("club_tag")?,
        score: r.try_get("score")?,
        submitted_at: ms(r.try_get::<OffsetDateTime, _>("submitted_at")?),
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
    })
}

/// Every website-listed row of every board of `family` in one statement (the hourly snapshot,
/// DASHBOARD_API.md §5): the family's grouped ranking ([`scores::ranking_ctes`], the game's
/// ranks), the hidden rows dropped after ranking, each row with its club tag; by board, then rank.
pub async fn family_rows_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    family: &Family<'_>,
) -> Result<Vec<(i32, WebRow)>, IdentityError> {
    let (ranking, _, _) = family.ranking();
    let column = ranking.board_column();
    let sql = format!(
        "WITH {ctes} \
         SELECT r.{column} AS board, r.position, r.profile_id, r.name, r.discord_name, r.avatar, r.score, \
           r.submitted_at, r.car_id, r.car_class_id, r.car_pi, r.is_clean, r.used_stm, r.used_tcs, r.used_abs, \
           r.used_auto_brake, r.used_auto_shifting, r.used_clutch, c.tag AS club_tag \
         FROM ranked r \
         LEFT JOIN game_club_member m ON m.game = $1 AND m.profile_id = r.profile_id \
         LEFT JOIN game_club c ON c.game = $1 AND c.id = m.club_id AND c.mode = $2 \
           AND c.privacy <> {private} \
         WHERE r.web_listed \
         ORDER BY r.{column}, r.position",
        ctes = scores::ranking_ctes(ranking, PLAYER),
        private = crate::clubs::PRIVACY_PRIVATE,
    );
    let rows = family
        .bind(sqlx::query(sqlx::AssertSqlSafe(sql)), game, mode)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| Ok((r.try_get("board")?, web_row(r, r.try_get("position")?)?)))
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// The game's #1 of each board of `family`, when listed on the website: `(board, profile, name,
/// score)` (a club page's records, DASHBOARD_API.md §6). The family's grouped ranking
/// ([`scores::ranking_ctes`]); one statement.
pub async fn family_tops_in(
    conn: &mut PgConnection,
    game: &str,
    mode: &str,
    family: &Family<'_>,
) -> Result<Vec<(i32, uuid::Uuid, String, f64)>, IdentityError> {
    let (ranking, _, _) = family.ranking();
    let column = ranking.board_column();
    let sql = format!(
        "WITH {ctes} \
         SELECT r.{column} AS board, r.profile_id, r.name, r.score FROM ranked r \
         WHERE r.position = 1 AND r.web_listed ORDER BY r.{column}",
        ctes = scores::ranking_ctes(ranking, PLAYER),
    );
    let rows = family
        .bind(sqlx::query(sqlx::AssertSqlSafe(sql)), game, mode)
        .fetch_all(&mut *conn)
        .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("board")?,
                r.try_get("profile_id")?,
                r.try_get("name")?,
                r.try_get("score")?,
            ))
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}
