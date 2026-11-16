//! Idle in game (OD-121, GATE_API.md §4a): applies the hook's activity reports to a play session
//! (`play_session.activity_*`, migration `20261025130000_idle_in_game.sql`) with the pure rule of
//! `fi_domain::activity`, and records each idle episode someone could replace
//! (`session_idle_observation`). With `Thresholds::evict` (`FI_IDLE_EVICT`) an idle session someone
//! could replace is warned for `warn_secs` (`evict_in` on its answers) and then ended `idle`
//! (`session.ended`); without it, episodes are only recorded.
//!
//! - [`report`]: one report, in its own short transaction: the session row locked alone and
//!   `SKIP LOCKED` (no pool or game lock, and never a wait on a row another decision holds: the
//!   game's request must not be delayed; the next report covers a skipped one). A report not newer
//!   than the session's last one stores nothing but still judges the stored track at now, so a
//!   client cannot freeze its state by repeating or inflating `seq`.
//! - [`evaluate`]: judges a session that reported before from its stored track at now (the gate
//!   calls it after a heartbeat), so a hook that stops reporting turns idle by time alone.
//! - [`skill_scored`]: a skill score submitted by the session is progress.
//! - [`observations`]: the owner's summary (`fi-cli idle-report`).

use crate::identity::{IdentityError, ms};
use fi_domain::activity::{self, Judged, Report, State, Thresholds, Track};
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// What a report did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The session's state after it.
    pub state: State,
    /// The rule's idle time (ms).
    pub idle_ms: i64,
    /// Sessions that could take this one's slot now (0 unless idle).
    pub waiting: i64,
    /// Whether this report recorded a new idle episode.
    pub observed: bool,
    /// Seconds left before the session is ended (enforcement, during the warning).
    pub evict_in: Option<u32>,
    /// Whether this judgement ended the session (`idle`).
    pub evicted: bool,
}

fn at(v: OffsetDateTime) -> i64 {
    ms(v).millis()
}

fn ts(millis: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

struct Row0 {
    game: String,
    tier: String,
    now: i64,
    track: Option<Track>,
}

/// The session's row, locked without waiting; `None` when it is not active or another decision
/// holds it.
async fn lock_in(
    conn: &mut sqlx::PgConnection,
    session: Uuid,
) -> Result<Option<Row0>, IdentityError> {
    let Some(r) = sqlx::query(
        "SELECT s.game, p.tier, s.activity_seq, s.activity_odo_base_m, s.activity_progress_at, \
           s.activity_engaged_at, clock_timestamp() AS now \
         FROM play_session s JOIN pool p ON p.name = s.pool \
         WHERE s.id = $1 AND s.state = 'active' FOR UPDATE OF s SKIP LOCKED",
    )
    .bind(session)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let now = at(r.try_get("now")?);
    let track = match r.try_get::<Option<i64>, _>("activity_seq")? {
        None => None,
        Some(seq) => Some(Track {
            seq: seq as u64,
            odo_base_m: r
                .try_get::<Option<i64>, _>("activity_odo_base_m")?
                .unwrap_or(0) as u64,
            progress_at: r
                .try_get::<Option<OffsetDateTime>, _>("activity_progress_at")?
                .map_or(now, at),
            engaged_at: r
                .try_get::<Option<OffsetDateTime>, _>("activity_engaged_at")?
                .map_or(now, at),
        }),
    };
    Ok(Some(Row0 {
        game: r.try_get("game")?,
        tier: r.try_get("tier")?,
        now,
        track,
    }))
}

/// Applies `report` to `session` at the database clock. `None` when it changed nothing (no such
/// active session, or its row is held by another decision). A report whose `seq` is not above the
/// last one stores nothing but is still judged from the stored track.
pub async fn report(
    db: &PgPool,
    session: Uuid,
    report: &Report,
    thresholds: &Thresholds,
) -> Result<Option<Applied>, IdentityError> {
    let mut tx = db.begin().await?;
    let Some(row) = lock_in(&mut tx, session).await? else {
        return Ok(None);
    };
    let judged = match row.track {
        Some(t) if report.seq <= t.seq => activity::classify(t, row.now, &row.tier, thresholds),
        previous => {
            let j = activity::judge(previous, report, false, row.now, &row.tier, thresholds);
            sqlx::query(
                "UPDATE play_session SET activity_seq = $2, activity_odo_base_m = $3, \
                   activity_progress_at = $4, activity_engaged_at = $5, activity_reported_at = $6, \
                   activity_rp = $7 WHERE id = $1",
            )
            .bind(session)
            .bind(j.track.seq as i64)
            .bind(j.track.odo_base_m as i64)
            .bind(ts(j.track.progress_at))
            .bind(ts(j.track.engaged_at))
            .bind(ts(row.now))
            .bind(i16::from(report.rp))
            .execute(&mut *tx)
            .await?;
            j
        }
    };
    let applied = observe_in(&mut tx, session, &row, &judged, thresholds).await?;
    tx.commit().await?;
    Ok(Some(applied))
}

/// Judges a session that reported before from its stored track at now (no report: a hook that
/// stopped sending turns idle by time). `None` for a session that never reported (`no_signal`),
/// is not active, or whose row another decision holds.
pub async fn evaluate(
    db: &PgPool,
    session: Uuid,
    thresholds: &Thresholds,
) -> Result<Option<Applied>, IdentityError> {
    let mut tx = db.begin().await?;
    let Some(row) = lock_in(&mut tx, session).await? else {
        return Ok(None);
    };
    let Some(track) = row.track else {
        return Ok(None);
    };
    let judged = activity::classify(track, row.now, &row.tier, thresholds);
    let applied = observe_in(&mut tx, session, &row, &judged, thresholds).await?;
    tx.commit().await?;
    Ok(Some(applied))
}

/// Records the episode of an idle session someone could replace: one row per episode (keyed by
/// the progress it started after), its idle, menu and waiting values raised to the largest seen.
/// Enforced (`thresholds.evict`): the episode's first such judgement starts the warning
/// (`warned_at`); once `warn_secs` have passed and someone still waits, the session is ended
/// `idle` under the row lock the caller holds (the sweeper's close, its `session.ended`). A session
/// that is no longer idle, or that nobody waits for, has its pending warning cleared, and so does
/// every judgement while enforcement is off.
async fn observe_in(
    conn: &mut sqlx::PgConnection,
    session: Uuid,
    row: &Row0,
    j: &Judged,
    thresholds: &Thresholds,
) -> Result<Applied, IdentityError> {
    let (mut waiting, mut observed, mut evict_in, mut evicted) = (0, false, None, false);
    // OD-180.2 (S29): a host whose game is away while its launcher serves the other players of
    // its world is not idle on purpose; nobody may take its slot from under them.
    if j.state == State::Idle && !crate::matchmaking::holds_away_world_in(conn, session).await? {
        waiting = could_replace_in(conn, &row.game, &row.tier).await?;
    }
    if waiting > 0 {
        let clamp = |v: i64| v.clamp(0, i64::from(i32::MAX)) as i32;
        let episode = sqlx::query(
            "INSERT INTO session_idle_observation \
               (game, session_id, tier, progress_at, idle_secs, menu_secs, waiting) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT ON CONSTRAINT session_idle_observation_episode DO UPDATE SET \
               idle_secs = GREATEST(session_idle_observation.idle_secs, EXCLUDED.idle_secs), \
               menu_secs = GREATEST(session_idle_observation.menu_secs, EXCLUDED.menu_secs), \
               waiting = GREATEST(session_idle_observation.waiting, EXCLUDED.waiting), \
               last_observed_at = clock_timestamp() \
             RETURNING id, (xmax = 0) AS fresh, warned_at, clock_timestamp() AS now",
        )
        .bind(&row.game)
        .bind(session)
        .bind(if row.tier == "vip" { "vip" } else { "free" })
        .bind(ts(j.track.progress_at))
        .bind(clamp(j.idle_ms / 1_000))
        .bind(clamp(j.menu_ms / 1_000))
        .bind(clamp(waiting))
        .fetch_one(&mut *conn)
        .await?;
        observed = episode.try_get("fresh")?;
        if !thresholds.evict {
            // Observation never keeps a warning: one left by an enforced revision (a rollback, or
            // revisions side by side during a rollout) must not end a session unwarned later.
            sqlx::query(
                "UPDATE session_idle_observation SET warned_at = NULL \
                 WHERE session_id = $1 AND warned_at IS NOT NULL AND NOT evicted",
            )
            .bind(session)
            .execute(&mut *conn)
            .await?;
        } else {
            let id: Uuid = episode.try_get("id")?;
            let now: OffsetDateTime = episode.try_get("now")?;
            let warn_ms = i64::from(thresholds.warn_secs) * 1_000;
            match episode.try_get::<Option<OffsetDateTime>, _>("warned_at")? {
                None => {
                    sqlx::query("UPDATE session_idle_observation SET warned_at = $2 WHERE id = $1")
                        .bind(id)
                        .bind(now)
                        .execute(&mut *conn)
                        .await?;
                    evict_in = Some(thresholds.warn_secs);
                }
                Some(warned) if at(now) - at(warned) < warn_ms => {
                    let left_ms = warn_ms - (at(now) - at(warned));
                    evict_in = Some(((left_ms + 999) / 1_000).clamp(1, i64::from(u32::MAX)) as u32);
                }
                Some(_) => {
                    evicted = crate::admission::close_idle_in(conn, session).await?;
                    if evicted {
                        sqlx::query(
                            "UPDATE session_idle_observation SET evicted = true WHERE id = $1",
                        )
                        .bind(id)
                        .execute(&mut *conn)
                        .await?;
                    }
                }
            }
        }
    } else {
        // Back to play, or nobody waits any more: a later idle stretch is warned afresh.
        sqlx::query(
            "UPDATE session_idle_observation SET warned_at = NULL \
             WHERE session_id = $1 AND warned_at IS NOT NULL AND NOT evicted",
        )
        .bind(session)
        .execute(&mut *conn)
        .await?;
    }
    Ok(Applied {
        state: j.state,
        idle_ms: j.idle_ms,
        waiting,
        observed,
        evict_in,
        evicted,
    })
}

/// Queued sessions of `game` that could take the slot of an idle session of `tier` (GATE_API.md
/// §4a, OD-71's order): for a free one, every live queued free player (the free slot it leaves is
/// theirs) and the queued Supporters their own pool has room for; for a Supporter's, the queued
/// Supporters its pool would have room for once it leaves.
async fn could_replace_in(
    conn: &mut sqlx::PgConnection,
    game: &str,
    tier: &str,
) -> Result<i64, IdentityError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(CASE \
             WHEN p.tier <> 'vip' THEN CASE WHEN $2 = 'vip' THEN 0 ELSE q.queued END \
             ELSE GREATEST(LEAST(q.queued, COALESCE(p.capacity - q.active + \
                    CASE WHEN $2 = 'vip' THEN 1 ELSE 0 END, q.queued)), 0) END), 0)::bigint \
         FROM pool p CROSS JOIN LATERAL ( \
           SELECT count(*) FILTER (WHERE s.state = 'queued' AND s.expires_at > clock_timestamp()) AS queued, \
                  count(*) FILTER (WHERE s.state = 'active') AS active \
           FROM play_session s WHERE s.game = p.game AND s.pool = p.name \
             AND s.state IN ('active', 'queued')) q \
         WHERE p.game = $1",
    )
    .bind(game)
    .bind(tier)
    .fetch_one(&mut *conn)
    .await?;
    Ok(n)
}

/// A skill score submitted by `session` (`Scoreboard.SubmitXpScore`): progress now, for a
/// session that reports activity. `true` when it moved the session's progress.
pub async fn skill_scored(db: &PgPool, session: Uuid) -> Result<bool, IdentityError> {
    Ok(sqlx::query(
        "UPDATE play_session SET activity_progress_at = clock_timestamp() \
         WHERE id = $1 AND state = 'active' AND activity_seq IS NOT NULL",
    )
    .bind(session)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

/// The owner's summary of idle episodes per tier since `days` days ago (`fi-cli idle-report`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    /// `free` or `vip`.
    pub tier: String,
    /// Episodes (each an idle session someone could have replaced).
    pub episodes: i64,
    /// Distinct sessions.
    pub sessions: i64,
    /// Median and 90th percentile of the idle seconds at the episode.
    pub idle_p50: i64,
    /// 90th percentile.
    pub idle_p90: i64,
    /// Median of the menu seconds credited.
    pub menu_p50: i64,
    /// Median of the sessions waiting.
    pub waiting_p50: i64,
}

/// [`Summary`] per tier of `game` over the last `days` (1 to 90).
pub async fn observations(
    db: &PgPool,
    game: &str,
    days: i32,
) -> Result<Vec<Summary>, IdentityError> {
    let rows = sqlx::query(
        "SELECT tier, count(*) AS episodes, count(DISTINCT session_id) AS sessions, \
           percentile_disc(0.5) WITHIN GROUP (ORDER BY idle_secs)::bigint AS idle_p50, \
           percentile_disc(0.9) WITHIN GROUP (ORDER BY idle_secs)::bigint AS idle_p90, \
           percentile_disc(0.5) WITHIN GROUP (ORDER BY menu_secs)::bigint AS menu_p50, \
           percentile_disc(0.5) WITHIN GROUP (ORDER BY waiting)::bigint AS waiting_p50 \
         FROM session_idle_observation \
         WHERE game = $1 AND observed_at > clock_timestamp() - make_interval(days => $2) \
         GROUP BY tier ORDER BY tier",
    )
    .bind(game)
    .bind(days.clamp(1, 90))
    .fetch_all(db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(Summary {
            tier: r.try_get("tier")?,
            episodes: r.try_get("episodes")?,
            sessions: r.try_get("sessions")?,
            idle_p50: r.try_get("idle_p50")?,
            idle_p90: r.try_get("idle_p90")?,
            menu_p50: r.try_get("menu_p50")?,
            waiting_p50: r.try_get("waiting_p50")?,
        });
    }
    Ok(out)
}
