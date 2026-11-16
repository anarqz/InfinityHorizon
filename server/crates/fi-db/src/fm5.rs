//! Forza Motorsport 5's career and scores (M7; OD-206, CR-S104; GAME_PLATFORM.md §3a "Forza
//! Motorsport 5", "Career and scores (M7)"; migration `20261107220000_fm5_career_scores.sql`).
//!
//! FM5's profile sync is stored by [`crate::gameplane`] under game `fm5` (`game_profile`,
//! `game_profile_table`, the checkpoint in `game_logon`); this module adds what FM5 alone needs:
//!
//! - [`sync_progress_in`]: the career's `profile_progress` numbers (level, XP, credits, cars) from
//!   its stored sync. Since M11 (CR-S105) a Noob career's save commits write the same row
//!   ([`crate::progression::fm5_save_committed_in`]): the newest write wins, column by column;
//! - [`submit_score_in`], [`best_scores_in`]: `Scoreboard.SubmitScore` keeping one best per career
//!   and key as the game compares (response spec §11.2), and `Scoreboard.GetBestScores` reading it.
//!
//! No integrity gate, guard score or progression fact reads or writes any of it (experimental,
//! OD-206). Every write is an upsert: a replayed request writes the same rows.

use crate::identity::IdentityError;
use crate::progression::number_within;
use serde_json::{Map, Value as Json};
use sqlx::types::Json as Jsonb;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// The game code of every row written here.
pub const GAME: &str = fi_domain::FM5;

/// SubmitScore's `type` of a time (lower is better).
pub const TIME: i32 = 0;
/// SubmitScore's `type` of a drift score (higher is better).
pub const DRIFT: i32 = 1;

/// The UpdateAllTables table whose rows are the career's cars.
pub const GARAGE_TABLE: &str = "garageCars";

/// Writes the career's `profile_progress` numbers from its stored FM5 sync: `level` and `xp` from
/// `playerRaceHistory[0]`, `credits` from `playerHistory[0]`, `cars` the stored `garageCars` rows.
/// A number the sync has not carried yet keeps the projection's (UpdateAll and UpdateAllTables each
/// bring half). `races`, `play_seconds` and `source_revision` are never touched (the session
/// recount's, [`crate::progression::refresh_profile`]).
pub async fn sync_progress_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
) -> Result<(), IdentityError> {
    sync_progress_of_in(conn, GAME, profile_id).await
}

/// [`sync_progress_in`] for any game whose sync has FH2's shape (`playerRaceHistory[0]`,
/// `playerHistory[0]`, `garageCars`): FM5's career, and since OD-239 an FH2 career without a save
/// line (Azimuth), whose numbers have no save commit to come from. Never called for a career with
/// a line: its numbers are the save commits' ([`crate::progression::save_committed_in`]).
pub async fn sync_progress_of_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<(), IdentityError> {
    // A number within the column's range, else NULL (never raises).
    let number =
        |path: &str, lo: &str, hi: &str| number_within(&format!("gp.data #> '{path}'"), lo, hi);
    let level = number("{playerRaceHistory,0,level}", "0", "2147483647");
    let xp = number("{playerRaceHistory,0,xp}", "0", "9223372036854775807");
    let credits = number("{playerHistory,0,credits}", "0", "9223372036854775807");
    let sql = format!(
        "WITH fresh AS ( \
           SELECT {level}::integer AS level, {xp}::bigint AS xp, {credits}::bigint AS credits, \
             (SELECT (SELECT count(*) FROM jsonb_object_keys(t.data))::integer \
                FROM game_profile_table t \
               WHERE t.game = $1 AND t.profile_id = $2 AND t.name = $3) AS cars \
           FROM (SELECT 1) one \
           LEFT JOIN game_profile gp ON gp.game = $1 AND gp.profile_id = $2) \
         INSERT INTO profile_progress AS p (profile_id, level, xp, credits, cars) \
         SELECT $2, f.level, f.xp, f.credits, f.cars FROM fresh f \
         ON CONFLICT (profile_id) DO UPDATE SET \
           level = COALESCE(EXCLUDED.level, p.level), xp = COALESCE(EXCLUDED.xp, p.xp), \
           credits = COALESCE(EXCLUDED.credits, p.credits), cars = COALESCE(EXCLUDED.cars, p.cars) \
         WHERE (p.level, p.xp, p.credits, p.cars) IS DISTINCT FROM \
           (COALESCE(EXCLUDED.level, p.level), COALESCE(EXCLUDED.xp, p.xp), \
            COALESCE(EXCLUDED.credits, p.credits), COALESCE(EXCLUDED.cars, p.cars))"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(profile_id)
        .bind(GARAGE_TABLE)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The key GetBestScores names (its parameters), shared by both of a race's bests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScoreKey {
    /// `scoreboardType`.
    pub scoreboard_type: i32,
    /// `gameScoreboardId` (-1 for a private race).
    pub game_scoreboard_id: i32,
    /// `trackId` (i16 on the wire).
    pub track_id: i16,
    /// `carId` (i16 on the wire).
    pub car_id: i16,
    /// `carClassId`.
    pub car_class_id: u8,
    /// `carDriveTypeId`.
    pub car_drive_type_id: u8,
}

/// One submitted score (`Scoreboard.SubmitScore`).
#[derive(Clone, Debug, PartialEq)]
pub struct Submitted<'a> {
    /// The career.
    pub profile_id: Uuid,
    /// [`TIME`] or [`DRIFT`].
    pub score_type: i32,
    /// Its key.
    pub key: ScoreKey,
    /// The score (finite, not negative).
    pub score: f32,
    /// `isClean`.
    pub is_clean: bool,
    /// The reply's `scoreId`: the request's id, stable across a replay.
    pub score_id: Uuid,
    /// The submitted `ScoreboardScoreData`.
    pub data: &'a Map<String, Json>,
}

/// A stored best: its score and whether it was clean.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Best {
    /// The score.
    pub score: f32,
    /// `isClean`.
    pub is_clean: bool,
}

/// Whether `new` replaces `best` as the game compares (response spec §11.2): clean beats dirty,
/// dirty never beats clean, and with the same flag a strictly lower time or a strictly higher
/// drift score. The statement of [`submit_score_in`] is the same rule.
pub fn beats(score_type: i32, new: Best, best: Best) -> bool {
    match (new.is_clean, best.is_clean) {
        (true, false) => true,
        (false, true) => false,
        _ if score_type == DRIFT => new.score > best.score,
        _ => new.score < best.score,
    }
}

/// `Scoreboard.SubmitScore`: stores `s` when the career has no score for its type and key or `s`
/// [`beats`] it. `true` when it was stored; an equal score (a replay) keeps the earlier row.
pub async fn submit_score_in(
    conn: &mut PgConnection,
    s: &Submitted<'_>,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "INSERT INTO fm5_score AS t (game, profile_id, score_type, scoreboard_type, game_scoreboard_id, \
           track_id, car_id, car_class_id, car_drive_type_id, score, is_clean, score_id, data) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
         ON CONFLICT (game, profile_id, score_type, scoreboard_type, game_scoreboard_id, track_id, \
           car_id, car_class_id, car_drive_type_id) DO UPDATE SET score = EXCLUDED.score, \
           is_clean = EXCLUDED.is_clean, score_id = EXCLUDED.score_id, data = EXCLUDED.data, \
           submitted_at = clock_timestamp() \
         WHERE (EXCLUDED.is_clean AND NOT t.is_clean) \
            OR (EXCLUDED.is_clean = t.is_clean AND CASE WHEN t.score_type = 1 \
                  THEN EXCLUDED.score > t.score ELSE EXCLUDED.score < t.score END)",
    )
    .bind(GAME)
    .bind(s.profile_id)
    .bind(s.score_type)
    .bind(s.key.scoreboard_type)
    .bind(s.key.game_scoreboard_id)
    .bind(i32::from(s.key.track_id))
    .bind(i32::from(s.key.car_id))
    .bind(i16::from(s.key.car_class_id))
    .bind(i16::from(s.key.car_drive_type_id))
    .bind(s.score)
    .bind(s.is_clean)
    .bind(s.score_id)
    .bind(Jsonb(s.data))
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// `Scoreboard.GetBestScores`: the career's stored best time and drift score for `key`, each `None`
/// when it has none.
pub async fn best_scores_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    key: ScoreKey,
) -> Result<(Option<Best>, Option<Best>), IdentityError> {
    let rows = sqlx::query(
        "SELECT score_type, score, is_clean FROM fm5_score \
         WHERE game = $1 AND profile_id = $2 AND scoreboard_type = $3 AND game_scoreboard_id = $4 \
           AND track_id = $5 AND car_id = $6 AND car_class_id = $7 AND car_drive_type_id = $8",
    )
    .bind(GAME)
    .bind(profile_id)
    .bind(key.scoreboard_type)
    .bind(key.game_scoreboard_id)
    .bind(i32::from(key.track_id))
    .bind(i32::from(key.car_id))
    .bind(i16::from(key.car_class_id))
    .bind(i16::from(key.car_drive_type_id))
    .fetch_all(&mut *conn)
    .await?;
    let (mut time, mut drift) = (None, None);
    for r in rows {
        let best = Best {
            score: r.try_get("score")?,
            is_clean: r.try_get("is_clean")?,
        };
        match r.try_get::<i32, _>("score_type")? {
            TIME => time = Some(best),
            DRIFT => drift = Some(best),
            _ => {}
        }
    }
    Ok((time, drift))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(score: f32, is_clean: bool) -> Best {
        Best { score, is_clean }
    }

    #[test]
    fn the_games_comparison() {
        // Clean beats dirty whatever the numbers; dirty never beats clean.
        assert!(beats(TIME, b(90.0, true), b(40.0, false)));
        assert!(!beats(TIME, b(10.0, false), b(40.0, true)));
        assert!(beats(DRIFT, b(1.0, true), b(9000.0, false)));
        assert!(!beats(DRIFT, b(9000.0, false), b(1.0, true)));
        // Same flag: a strictly lower time, a strictly higher drift score.
        assert!(beats(TIME, b(41.0, false), b(42.29, false)));
        assert!(!beats(TIME, b(43.0, false), b(42.29, false)));
        assert!(!beats(TIME, b(42.29, true), b(42.29, true)));
        assert!(beats(DRIFT, b(2.0, true), b(1.0, true)));
        assert!(!beats(DRIFT, b(1.0, false), b(1.0, false)));
    }
}
