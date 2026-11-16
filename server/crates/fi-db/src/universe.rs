//! The Supporters' universe rows of `/saves/v1` (migration `20261019000300_universe.sql`; ADR
//! 0016; docs/contracts/UNIVERSE_API.md): positions, read by `nearby` through one indexed query,
//! and the CR-L14 race-time refusals, both written by the progress handler in the same transaction
//! as the fact (`fi_db::game::record_progression_in`).
//!
//! The leaderboards, rivals and ghosts of this API (`board_entry`, `ghost`) were retired on
//! 2026-10-27 (migration `20261027180000_retire_universe_boards.sql`): the game's own boards and
//! ghosts are the Forza RPC's (`fi_db::scores`: `game_score`, `game_ghost`).
//!
//! - CR-L14 plausibility (CLOUD_SAVES.md §7.1): `min_time` (a race faster than [`MIN_RACE_MS`])
//!   refuses a value once per line: the refused value is recorded in `universe_refusal` and the
//!   fact stays; the same or a less extreme value is never refused by that rule again.
//! - Positions are last-writer-wins per profile, bucketed by [`cell`]; [`nearby`] reads the cells
//!   the radius touches and filters the exact distance in SQL.
//!
//! Every read is a single statement on the pool; every write takes the caller's connection.

use crate::identity::{IdentityError, ms};
use fi_domain::UnixMs;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// Grid cell size as a shift: cells are 1024 map units square.
pub const CELL_SHIFT: u32 = 10;
/// A race time below this is refused once per line (`min_time`).
pub const MIN_RACE_MS: i64 = 1_000;
/// Positions older than this are not nearby.
pub const STALE_SECS: i64 = 120;

/// The packed grid cell of a map point (`x >> 10` in the high half, `y >> 10` in the low half).
pub fn cell(x: i64, y: i64) -> i64 {
    let (cx, cy) = (x >> CELL_SHIFT, y >> CELL_SHIFT);
    (cx << 32) | (cy & 0xffff_ffff)
}

/// Every cell the square `[x - r, x + r] × [y - r, y + r]` touches: the caller's cell and its
/// neighbours (9 cells for a radius under 1024; at most 121 at the contract's 5000).
pub fn cells_around(x: i64, y: i64, radius: i64) -> Vec<i64> {
    let (x0, x1) = ((x - radius) >> CELL_SHIFT, (x + radius) >> CELL_SHIFT);
    let (y0, y1) = ((y - radius) >> CELL_SHIFT, (y + radius) >> CELL_SHIFT);
    let mut out = Vec::new();
    for cx in x0..=x1 {
        for cy in y0..=y1 {
            out.push((cx << 32) | (cy & 0xffff_ffff));
        }
    }
    out
}

/// Who writes: a profile's live session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writer {
    /// `fh2`.
    pub game: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The profile.
    pub profile_id: Uuid,
    /// Its play session.
    pub session_id: Uuid,
}

/// What a fact did beyond its own row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Accepted (nothing else to write, or a value already refused once on this line).
    Applied,
    /// A plausibility rule refused the value: `(rule, field)`.
    Refused(&'static str, &'static str),
}

/// CR-L14 `min_time` for a `race_result` of `writer` on `event` (fact `event_id`): a time under
/// [`MIN_RACE_MS`] is refused once per line, the refusal kept in `universe_refusal` for review.
pub async fn race_time_in(
    conn: &mut PgConnection,
    writer: &Writer,
    event: &str,
    time_ms: i64,
    event_id: Uuid,
) -> Result<Applied, IdentityError> {
    if time_ms >= MIN_RACE_MS {
        return Ok(Applied::Applied);
    }
    // The fastest time refused so far by this rule on this line, if any.
    let refused: Option<i64> = sqlx::query_scalar(
        "SELECT value FROM universe_refusal WHERE game = $1 AND profile_id = $2 AND board = $3 AND rule = 'min_time' FOR UPDATE",
    )
    .bind(&writer.game)
    .bind(writer.profile_id)
    .bind(event)
    .fetch_optional(&mut *conn)
    .await?;
    if refused.is_some_and(|r| time_ms >= r) {
        return Ok(Applied::Applied);
    }
    sqlx::query(
        "INSERT INTO universe_refusal (game, profile_id, board, rule, value, event_id) VALUES ($1, $2, $3, 'min_time', $4, $5) \
         ON CONFLICT (game, profile_id, board, rule) DO UPDATE SET value = EXCLUDED.value, event_id = EXCLUDED.event_id, refused_at = clock_timestamp()",
    )
    .bind(&writer.game)
    .bind(writer.profile_id)
    .bind(event)
    .bind(time_ms)
    .bind(event_id)
    .execute(&mut *conn)
    .await?;
    Ok(Applied::Refused("min_time", "timeMs"))
}

/// Upserts the writer's position; `false` when `event_id` already wrote it (a replay).
pub async fn position_in(
    conn: &mut PgConnection,
    writer: &Writer,
    event_id: Uuid,
    x: i32,
    y: i32,
    heading: Option<i16>,
    car: Option<i32>,
) -> Result<bool, IdentityError> {
    let written: Option<i32> = sqlx::query_scalar(
        "INSERT INTO player_position (game, profile_id, mode, cell, x, y, heading, car, session_id, last_event_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (game, profile_id) DO UPDATE SET mode = EXCLUDED.mode, cell = EXCLUDED.cell, x = EXCLUDED.x, y = EXCLUDED.y, \
           heading = EXCLUDED.heading, car = EXCLUDED.car, session_id = EXCLUDED.session_id, last_event_id = EXCLUDED.last_event_id, \
           updated_at = clock_timestamp() \
         WHERE player_position.last_event_id <> EXCLUDED.last_event_id \
         RETURNING 1",
    )
    .bind(&writer.game)
    .bind(writer.profile_id)
    .bind(&writer.mode)
    .bind(cell(i64::from(x), i64::from(y)))
    .bind(x)
    .bind(y)
    .bind(heading)
    .bind(car)
    .bind(writer.session_id)
    .bind(event_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(written.is_some())
}

/// A nearby player.
#[derive(Clone, Debug, PartialEq)]
pub struct NearbyRow {
    /// Profile.
    pub profile_id: Uuid,
    /// XUID.
    pub xuid: i64,
    /// `Name#12345`.
    pub gamertag: String,
    /// Map x.
    pub x: i32,
    /// Map y.
    pub y: i32,
    /// Euclidean distance from the query point.
    pub distance: f64,
    /// Car.
    pub car: Option<i32>,
    /// Reported at.
    pub updated_at: UnixMs,
}

/// The SQL of [`nearby`] (tests EXPLAIN it).
pub const NEARBY_SQL: &str = "SELECT pp.profile_id, p.xuid, p.name AS gamertag, \
       pp.x, pp.y, pp.car, pp.updated_at, \
       sqrt(((pp.x - $4)::float8 * (pp.x - $4)::float8) + ((pp.y - $5)::float8 * (pp.y - $5)::float8)) AS distance \
     FROM player_position pp JOIN profile p ON p.id = pp.profile_id \
     WHERE pp.game = $1 AND pp.mode = $2 AND pp.cell = ANY($3) \
       AND pp.updated_at > now() - make_interval(secs => $8) \
       AND pp.profile_id <> $6 \
       AND (pp.x - $4)::bigint * (pp.x - $4)::bigint + (pp.y - $5)::bigint * (pp.y - $5)::bigint <= $7::bigint * $7::bigint \
     ORDER BY distance, pp.profile_id LIMIT $9";

/// Players within `radius` of `(x, y)` in `game` and `mode`, fresher than [`STALE_SECS`],
/// nearest first, `except` left out.
#[allow(clippy::too_many_arguments)]
pub async fn nearby(
    db: &PgPool,
    game: &str,
    mode: &str,
    x: i64,
    y: i64,
    radius: i64,
    except: Uuid,
    limit: u32,
) -> Result<Vec<NearbyRow>, IdentityError> {
    let rows = sqlx::query(NEARBY_SQL)
        .bind(game)
        .bind(mode)
        .bind(cells_around(x, y, radius))
        .bind(x)
        .bind(y)
        .bind(except)
        .bind(radius)
        .bind(STALE_SECS as f64)
        .bind(i64::from(limit))
        .fetch_all(db)
        .await?;
    rows.iter()
        .map(|r| {
            Ok(NearbyRow {
                profile_id: r.try_get("profile_id")?,
                xuid: r.try_get("xuid")?,
                gamertag: r
                    .try_get::<Option<String>, _>("gamertag")?
                    .unwrap_or_default(),
                x: r.try_get("x")?,
                y: r.try_get("y")?,
                distance: r.try_get("distance")?,
                car: r.try_get("car")?,
                updated_at: ms(r.try_get("updated_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Every game code.
pub async fn games(db: &PgPool) -> Result<Vec<String>, IdentityError> {
    Ok(sqlx::query_scalar("SELECT code FROM game ORDER BY code")
        .fetch_all(db)
        .await?)
}

/// The public totals of `game` computed from Postgres: the `totals` snapshot `fi-cli
/// consolidate-nightly` writes (no BigQuery gold build any more, owner 2026-10-26). The boards and
/// ghosts are the game's own (`game_score`, `game_ghost`; the universe's were retired 2026-10-27).
pub async fn local_totals(db: &PgPool, game: &str) -> Result<serde_json::Value, IdentityError> {
    let row = sqlx::query(
        "SELECT (SELECT count(*) FROM profile WHERE game = $1 AND status = 'active') AS profiles, \
           (SELECT count(*) FROM game_score WHERE game = $1) AS board_entries, \
           (SELECT count(*) FROM (SELECT DISTINCT scoreboard_type, score_type, game_scoreboard_id, track_id \
              FROM game_score WHERE game = $1) b) AS boards, \
           (SELECT count(*) FROM game_ghost WHERE game = $1) AS ghosts, \
           (SELECT count(*) FROM world WHERE game = $1 AND closed_at IS NULL AND expires_at > clock_timestamp()) AS open_worlds",
    )
    .bind(game)
    .fetch_one(db)
    .await?;
    let n = |c: &str| -> Result<i64, sqlx::Error> { row.try_get(c) };
    Ok(serde_json::json!({
        "profiles": n("profiles")?,
        "boardEntries": n("board_entries")?,
        "boards": n("boards")?,
        "ghosts": n("ghosts")?,
        "openWorlds": n("open_worlds")?,
        "source": "postgres",
    }))
}

/// Replaces one gold snapshot row (fi-cli consolidate-nightly, a maintenance login).
pub async fn put_gold_snapshot(
    db: &PgPool,
    game: &str,
    name: &str,
    payload: &serde_json::Value,
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO gold_snapshot (game, name, built_at, payload) VALUES ($1, $2, clock_timestamp(), $3) \
         ON CONFLICT (game, name) DO UPDATE SET built_at = EXCLUDED.built_at, payload = EXCLUDED.payload",
    )
    .bind(game)
    .bind(name)
    .bind(payload)
    .execute(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_pack_and_neighbours_cover_the_radius() {
        assert_eq!(cell(0, 0), 0);
        assert_eq!(cell(1023, 1023), 0);
        assert_eq!(cell(1024, 0), 1 << 32);
        assert_eq!(cell(-1, -1), (-1i64 << 32) | 0xffff_ffff);
        assert_ne!(cell(-1, 0), cell(0, -1));
        assert_eq!(cells_around(512, 512, 100), vec![0], "inside one cell");
        assert_eq!(
            cells_around(1000, 1000, 100).len(),
            4,
            "on a corner: two per axis"
        );
        assert_eq!(
            cells_around(512, 512, 1023).len(),
            9,
            "the cell and its neighbours"
        );
        assert_eq!(cells_around(0, 0, 5000).len(), 100);
        assert!(cells_around(1023, 1023, 5000).len() <= 121);
        // Every point within the radius sits in one of the cells.
        for (px, py) in [(-1100, 0), (0, -1100), (1100, 1100), (2000, -200)] {
            let r = 2500;
            assert!(cells_around(0, 0, r).contains(&cell(px, py)), "{px},{py}");
        }
    }
}
