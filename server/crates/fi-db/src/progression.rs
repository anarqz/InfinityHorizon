//! The progression read models (`profile_progress`, `session_summary`) over the game tables.
//! No outbox and no push consumer (owner-decisions, OD-122 follow-ups 2026-10-26; ADR 0007
//! superseded): the writers update the read models in their own transaction, and the nightly job
//! rebuilds whatever they missed.
//!
//! - **Synchronous, in the writer's transaction, inside a savepoint:** a save commit copies its
//!   revision's numbers ([`save_committed_guarded`], `fi_db::saves::commit_in`); every session
//!   close builds its `session_summary` and recounts the profile ([`session_ended_guarded`],
//!   `fi_db::admission`, `fi_db::moderation`); a race fact ([`race_fact`]) recounts the profile
//!   ([`refresh_profile_guarded`], `fi_db::game`). A read-model failure rolls back to the
//!   savepoint and is logged (ids only): it never blocks the close, the commit or the fact, and the
//!   nightly run repairs it. Every number read from JSON is range-checked in SQL
//!   ([`number_within`]), so no stored value can make these statements raise.
//! - **Nightly** ([`consolidate_nightly`], `fi-cli consolidate-nightly`): `profile_progress` from
//!   `progression_event` up to a watermark behind the clock, the `session_summary` of every ended
//!   session without one, and the head's numbers of every profile behind its save head.
//!
//! **The game tables decide.** The nightly job runs beside the writers, so every per-profile sum is
//! written as an *absolute recount* from Postgres under the `profile_progress` row lock
//! ([`refresh_profile`]), never as `+= 1`: two writers that both see a fact can never count it
//! twice, and whichever runs last leaves the exact value. Lock order: a writer holds its
//! `save_head` (or pool) lock and then takes the `profile_progress` row; the nightly job takes only
//! the `profile_progress` row, so there is no cycle. Concretely:
//!
//! | Column | Value |
//! | --- | --- |
//! | `races` | the profile's race facts ([`race_fact`]): accepted `score` facts on `scoreboardType` 1 (`board` `"1:*"`, `accepted: true`; the game's `Scoreboard.SubmitScore` of a finished race) plus accepted `race_result` facts (`accepted` absent or not `false`) |
//! | `play_seconds` | Σ `ended_at − admitted_at` over the profile's closed, admitted sessions |
//! | `last_session_id` | the profile's most recently ended session |
//! | `credits`, `xp`, `level`, `cars`, `source_revision` | the numbers of one revision, written only by a newer one (`source_revision < $revision`): the commit from its summary, the session end and the nightly from `save_revision.summary` of `end_revision` / the head |
//! | `consolidated_through` | the nightly watermark (only moves forward) |
//!
//! `session_summary` is built once per session (`ON CONFLICT DO NOTHING`) from `play_session` and
//! the session's facts: `started_at = admitted_at`, else `queued_at`; `events` = the session's
//! `progression_event` rows; `races` = its race facts ([`race_fact`]); `xp_gained` = Σ the
//! `amount` of its accepted `xp` facts; `credits_delta` = credits at `end_revision` − credits at
//! `start_revision` (NULL when either is unknown).
//!
//! A progression fact's `payload` may be the kind's `data` object or the whole
//! `progression.recorded` payload: the readers take `payload->'data'` when present.

use crate::DbError;
use fi_saves::SaveSummary;
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// SQL: the fact object of a `progression_event` row aliased `e`.
pub(crate) const FACT: &str = "COALESCE(e.payload->'data', e.payload)";

/// SQL: whether the `progression_event` row aliased `e` is one finished race (CORE_API.md §2.4,
/// EVENTS.md §3.3). The game reports a finished race through `Scoreboard.SubmitScore`: an accepted
/// `score` fact whose `board` is on `scoreboardType` 1 (`"1:<scoreType>"`). An accepted
/// `race_result` fact (`/game/v1`, `/saves/v1/progress`; no FH2 client sends one) counts too. One
/// race is one SubmitScore, and no client sends a `race_result` for it, so nothing counts twice.
pub(crate) fn race_fact() -> String {
    format!(
        "((e.kind = 'race_result' AND ({FACT}->>'accepted') IS DISTINCT FROM 'false') \
           OR (e.kind = 'score' AND ({FACT}->>'board') LIKE '1:%' \
             AND ({FACT}->>'accepted') = 'true'))"
    )
}

/// Whether a fact of `kind` with `data` may count as a race ([`race_fact`]): its writer then
/// recounts `profile_progress.races`. It may say yes too often (a refused `race_result`), never too
/// rarely: the recount itself applies [`race_fact`].
pub fn is_race_fact(kind: &str, data: &serde_json::Map<String, serde_json::Value>) -> bool {
    let data = match data.get("data") {
        Some(serde_json::Value::Object(inner)) => inner,
        _ => data,
    };
    let text = |k: &str| match data.get(k) {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(v @ serde_json::Value::Bool(_)) => Some(v.to_string()),
        _ => None,
    };
    match kind {
        "race_result" => true,
        "score" => {
            text("accepted").as_deref() == Some("true")
                && text("board").is_some_and(|b| b.starts_with("1:"))
        }
        _ => false,
    }
}

/// SQL: the jsonb value `json` as a number within `lo..=hi`, else NULL. Never raises: a string, a
/// non-finite or a huge value stored by an older build (or edited in) reads as unknown, so no
/// aggregate here can fail a session close or the nightly run (CR-S55 review).
pub(crate) fn number_within(json: &str, lo: &str, hi: &str) -> String {
    format!(
        "(CASE WHEN jsonb_typeof({json}) = 'number' AND ({json})::text::numeric BETWEEN {lo} AND {hi} \
           THEN ({json})::text::numeric END)"
    )
}

/// SQL: a numeric expression clamped into `bigint`; NULL stays NULL (`GREATEST` and `LEAST`
/// skip NULLs, so it is tested first).
pub(crate) fn clamp_bigint(expr: &str) -> String {
    format!(
        "(SELECT CASE WHEN v IS NULL THEN NULL \
           ELSE LEAST(GREATEST(v, -9223372036854775808), 9223372036854775807)::bigint END \
         FROM (SELECT ({expr})::numeric AS v) c)"
    )
}

const I64_MIN: &str = "-9223372036854775808";
pub(crate) const I64_MAX: &str = "9223372036854775807";
const I32_MIN: &str = "-2147483648";
const I32_MAX: &str = "2147483647";

/// A save commit's numbers (`revision` of `profile`, its `summary`) into `profile_progress`, in
/// the commit's transaction; an older revision never overwrites a newer one.
pub async fn save_committed_in(
    conn: &mut PgConnection,
    profile: Uuid,
    revision: i64,
    summary: &SaveSummary,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO profile_progress (profile_id, credits, xp, level, cars, source_revision) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (profile_id) DO UPDATE SET credits = EXCLUDED.credits, xp = EXCLUDED.xp, \
           level = EXCLUDED.level, cars = EXCLUDED.cars, source_revision = EXCLUDED.source_revision \
         WHERE profile_progress.source_revision < EXCLUDED.source_revision",
    )
    .bind(profile)
    .bind(summary.credits)
    .bind(summary.xp)
    .bind(summary.level.and_then(|l| i32::try_from(l).ok()))
    .bind(summary.cars.and_then(|c| i32::try_from(c).ok()))
    .bind(revision)
    .execute(conn)
    .await?;
    Ok(())
}

/// A closed session (`ended_at` set in the caller's transaction) into the read models: its
/// `session_summary`, the profile's recount and, with `end_revision`, that revision's numbers.
pub async fn session_ended_in(
    conn: &mut PgConnection,
    session: Uuid,
    profile: Uuid,
    end_revision: Option<i64>,
) -> Result<(), sqlx::Error> {
    insert_summary(conn, session).await?;
    refresh_profile(conn, profile).await?;
    if let Some(end) = end_revision {
        copy_save_numbers(conn, profile, end).await?;
    }
    Ok(())
}

/// Runs a read-model write inside a `SAVEPOINT` of the caller's transaction (CR-S55 review: a
/// read model must never block a session close or a save commit). On an error it rolls back to
/// the savepoint, logs a warning with ids only and answers `Ok`: `consolidate-nightly` repairs the
/// read model. On a connection outside a transaction (autocommit) it runs the write as is and
/// only logs a failure. Only a failure of the savepoint statements themselves is an error.
macro_rules! guarded {
    ($conn:expr, $what:literal, $profile:expr, $call:expr) => {{
        let savepoint = sqlx::query("SAVEPOINT fi_read_model")
            .execute(&mut *$conn)
            .await;
        match savepoint {
            Ok(_) => {}
            // Autocommit (a caller's plain connection): every statement commits on its own, so
            // there is nothing to roll back; a failure is only logged.
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("25P01") => {
                if let Err(e) = $call.await {
                    tracing::warn!(profile = %$profile, error = %e, concat!("progression.read_model_skipped.", $what));
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        match $call.await {
            Ok(_) => {
                sqlx::query("RELEASE SAVEPOINT fi_read_model")
                    .execute(&mut *$conn)
                    .await?;
            }
            Err(e) => {
                tracing::warn!(profile = %$profile, error = %e, concat!("progression.read_model_skipped.", $what));
                sqlx::query("ROLLBACK TO SAVEPOINT fi_read_model")
                    .execute(&mut *$conn)
                    .await?;
                sqlx::query("RELEASE SAVEPOINT fi_read_model")
                    .execute(&mut *$conn)
                    .await?;
            }
        }
        Ok(())
    }};
}

/// [`save_committed_in`] that never fails the commit ([`guarded!`]).
pub async fn save_committed_guarded(
    conn: &mut PgConnection,
    profile: Uuid,
    revision: i64,
    summary: &SaveSummary,
) -> Result<(), sqlx::Error> {
    guarded!(
        conn,
        "save_committed",
        profile,
        save_committed_in(&mut *conn, profile, revision, summary)
    )
}

/// An FM5 save commit's numbers into `profile_progress` (CR-S105), sharing the row with the
/// career's online sync ([`crate::fm5::sync_progress_in`], CR-S104). **The newest write wins**,
/// column by column: the commit writes the numbers its save carries (a `None` keeps the row's,
/// as the sync's writer does), the sync writes the numbers its `UpdateAll`/`UpdateAllTables`
/// carry whenever they arrive, and neither reads the other's time. Revisions still order commits
/// among themselves: an older revision never overwrites a newer one (`source_revision`, which the
/// sync never moves), so a restore, a new revision, writes the restored numbers.
pub async fn fm5_save_committed_in(
    conn: &mut PgConnection,
    profile: Uuid,
    revision: i64,
    summary: &SaveSummary,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO profile_progress AS p (profile_id, credits, xp, level, cars, source_revision) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (profile_id) DO UPDATE SET credits = COALESCE(EXCLUDED.credits, p.credits), \
           xp = COALESCE(EXCLUDED.xp, p.xp), level = COALESCE(EXCLUDED.level, p.level), \
           cars = COALESCE(EXCLUDED.cars, p.cars), source_revision = EXCLUDED.source_revision \
         WHERE p.source_revision < EXCLUDED.source_revision",
    )
    .bind(profile)
    .bind(summary.credits)
    .bind(summary.xp)
    .bind(summary.level.and_then(|l| i32::try_from(l).ok()))
    .bind(summary.cars.and_then(|c| i32::try_from(c).ok()))
    .bind(revision)
    .execute(conn)
    .await?;
    Ok(())
}

/// [`fm5_save_committed_in`] that never fails the commit ([`guarded!`]).
pub async fn fm5_save_committed_guarded(
    conn: &mut PgConnection,
    profile: Uuid,
    revision: i64,
    summary: &SaveSummary,
) -> Result<(), sqlx::Error> {
    guarded!(
        conn,
        "fm5_save_committed",
        profile,
        fm5_save_committed_in(&mut *conn, profile, revision, summary)
    )
}

/// [`session_ended_in`] that never fails the session's close ([`guarded!`]).
pub async fn session_ended_guarded(
    conn: &mut PgConnection,
    session: Uuid,
    profile: Uuid,
    end_revision: Option<i64>,
) -> Result<(), sqlx::Error> {
    guarded!(
        conn,
        "session_ended",
        profile,
        session_ended_in(&mut *conn, session, profile, end_revision)
    )
}

/// [`refresh_profile`] that never fails the fact's write ([`guarded!`]).
pub async fn refresh_profile_guarded(
    conn: &mut PgConnection,
    profile: Uuid,
) -> Result<(), sqlx::Error> {
    guarded!(
        conn,
        "refresh_profile",
        profile,
        refresh_profile(&mut *conn, profile)
    )
}

/// Builds the `session_summary` of an ended session unless it exists; `true` when it was built.
pub async fn insert_summary(conn: &mut PgConnection, session: Uuid) -> Result<bool, sqlx::Error> {
    let amount = number_within(
        &format!("{FACT}->'amount'"),
        "0",
        &fi_gamedb::gates::XP_PER_REPORT.to_string(),
    );
    let end_credits = number_within("r.summary->'credits'", I64_MIN, I64_MAX);
    let xp_gained = clamp_bigint(&format!(
        "(SELECT sum({amount}) FROM progression_event e WHERE e.play_session_id = s.id \
           AND e.kind = 'xp' AND ({FACT}->>'accepted') IS DISTINCT FROM 'false')"
    ));
    let credits_delta = clamp_bigint(&format!(
        "(SELECT {end_credits} FROM save_revision r \
            WHERE r.profile_id = s.profile_id AND r.revision = s.end_revision) \
           - (SELECT {end_credits} FROM save_revision r \
            WHERE r.profile_id = s.profile_id AND r.revision = s.start_revision)"
    ));
    let sql = format!(
        "INSERT INTO session_summary (play_session_id, profile_id, started_at, ended_at, \
           start_revision, end_revision, events, xp_gained, credits_delta, races) \
         SELECT s.id, s.profile_id, \
           LEAST(COALESCE(s.admitted_at, s.queued_at, s.ended_at), s.ended_at), s.ended_at, \
           s.start_revision, s.end_revision, \
           (SELECT count(*) FROM progression_event e WHERE e.play_session_id = s.id), \
           {xp_gained}, {credits_delta}, \
           (SELECT count(*) FROM progression_event e WHERE e.play_session_id = s.id \
              AND {race}) \
         FROM play_session s WHERE s.id = $1 AND s.ended_at IS NOT NULL \
         ON CONFLICT (play_session_id) DO NOTHING",
        race = race_fact()
    );
    let done = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(session)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() == 1)
}

/// Recounts the profile's sums under its `profile_progress` row lock (module docs).
pub async fn refresh_profile(conn: &mut PgConnection, profile: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO profile_progress (profile_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    // The lock first, the recount in a later statement: under READ COMMITTED that statement's
    // snapshot is taken after the lock, so it sees every fact a previous holder saw.
    sqlx::query("SELECT 1 FROM profile_progress WHERE profile_id = $1 FOR UPDATE")
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    let sql = format!(
        "WITH fresh AS ( \
           SELECT \
             (SELECT count(*) FROM progression_event e WHERE e.profile_id = $1 \
                AND {race})::integer AS races, \
             (SELECT COALESCE(sum(floor(extract(epoch FROM s.ended_at - s.admitted_at))), 0)::bigint \
                FROM play_session s WHERE s.profile_id = $1 AND s.ended_at IS NOT NULL \
                AND s.admitted_at IS NOT NULL AND s.ended_at >= s.admitted_at) AS play_seconds, \
             (SELECT s.id FROM play_session s WHERE s.profile_id = $1 AND s.ended_at IS NOT NULL \
                ORDER BY s.ended_at DESC, s.id DESC LIMIT 1) AS last_session_id) \
         UPDATE profile_progress p SET races = f.races, play_seconds = f.play_seconds, \
           last_session_id = COALESCE(f.last_session_id, p.last_session_id) \
         FROM fresh f \
         WHERE p.profile_id = $1 AND (p.races, p.play_seconds, p.last_session_id) \
           IS DISTINCT FROM (f.races, f.play_seconds, COALESCE(f.last_session_id, p.last_session_id))",
        race = race_fact()
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(profile)
        .execute(conn)
        .await?;
    Ok(())
}

/// Copies the numbers of `revision` (from `save_revision.summary`) when it is newer than what the
/// projection reflects; never lowers `source_revision`. `true` when it moved.
pub async fn copy_save_numbers(
    conn: &mut PgConnection,
    profile: Uuid,
    revision: i64,
) -> Result<bool, sqlx::Error> {
    let credits = number_within("r.summary->'credits'", I64_MIN, I64_MAX);
    let xp = number_within("r.summary->'xp'", I64_MIN, I64_MAX);
    let level = number_within("r.summary->'level'", I32_MIN, I32_MAX);
    let cars = number_within("r.summary->'cars'", I32_MIN, I32_MAX);
    let sql = format!(
        "UPDATE profile_progress p SET \
           credits = {credits}::bigint, xp = {xp}::bigint, \
           level = {level}::integer, cars = {cars}::integer, \
           source_revision = r.revision \
         FROM save_revision r \
         WHERE p.profile_id = $1 AND r.profile_id = $1 AND r.revision = $2 \
           AND p.source_revision < r.revision"
    );
    let done = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(profile)
        .bind(revision)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() == 1)
}

/// What [`consolidate_nightly`] did (the `progression.nightly.summary` line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NightlyReport {
    /// `now − lag` by the database clock.
    pub watermark: OffsetDateTime,
    /// Missing `session_summary` rows built.
    pub summaries_built: u64,
    /// Profiles whose sums were recounted.
    pub profiles_refreshed: u64,
    /// Profiles whose save-derived numbers moved to the head's revision.
    pub heads_copied: u64,
    /// Sessions or profiles whose step failed (logged with ids, rolled back, skipped): the run
    /// goes on and the job exits non-zero at the end.
    pub failed: u64,
}

/// The nightly consolidation (module docs): one short transaction per session or
/// profile, so a long run never holds locks the services need.
///
/// 1. every session that ended before the watermark without a `session_summary` gets one, and its
///    profile is recounted;
/// 2. every profile with facts received in `[consolidated_through, watermark)` is recounted and
///    its `consolidated_through` moves to the watermark (never back);
/// 3. every profile whose save head is newer than `profile_progress.source_revision` takes the
///    head's numbers (never lowering `source_revision`).
///
/// A failed session or profile is logged (ids only), rolled back and counted in
/// [`NightlyReport::failed`]; the run goes on.
pub async fn consolidate_nightly(pool: &PgPool, lag_ms: u64) -> Result<NightlyReport, DbError> {
    let watermark: OffsetDateTime = sqlx::query_scalar(
        "SELECT clock_timestamp() - make_interval(secs => $1::double precision / 1000)",
    )
    .bind(lag_ms as f64)
    .fetch_one(pool)
    .await?;
    let mut report = NightlyReport {
        watermark,
        summaries_built: 0,
        profiles_refreshed: 0,
        heads_copied: 0,
        failed: 0,
    };

    let sessions = sqlx::query(
        // Never for a deleted profile: its erasure deleted its progress and facts, and a summary
        // here would write `profile_progress` for the tombstone again (PD-Q1 review).
        "SELECT s.id, s.profile_id FROM play_session s \
         WHERE s.ended_at IS NOT NULL AND s.ended_at < $1 \
           AND NOT EXISTS (SELECT 1 FROM session_summary m WHERE m.play_session_id = s.id) \
           AND NOT EXISTS (SELECT 1 FROM profile p WHERE p.id = s.profile_id AND p.status = 'deleted') \
         ORDER BY s.ended_at",
    )
    .bind(watermark)
    .fetch_all(pool)
    .await?;
    for row in &sessions {
        let session: Uuid = row.try_get("id")?;
        let profile: Uuid = row.try_get("profile_id")?;
        match summarize_one(pool, session, profile).await {
            Ok(built) => report.summaries_built += u64::from(built),
            Err(e) => {
                report.failed += 1;
                tracing::warn!(%session, %profile, error = %e, "progression.nightly.session_failed");
            }
        }
    }

    let profiles: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT e.profile_id FROM progression_event e \
         LEFT JOIN profile_progress p ON p.profile_id = e.profile_id \
         WHERE e.received_at < $1 \
           AND (p.consolidated_through IS NULL OR e.received_at >= p.consolidated_through)",
    )
    .bind(watermark)
    .fetch_all(pool)
    .await?;
    for profile in profiles {
        match recount_one(pool, profile, watermark).await {
            Ok(()) => report.profiles_refreshed += 1,
            Err(e) => {
                report.failed += 1;
                tracing::warn!(%profile, error = %e, "progression.nightly.profile_failed");
            }
        }
    }

    let heads = sqlx::query(
        "SELECT h.profile_id, h.revision FROM save_head h \
         LEFT JOIN profile_progress p ON p.profile_id = h.profile_id \
         WHERE h.revision > 0 AND h.revision > COALESCE(p.source_revision, 0)",
    )
    .fetch_all(pool)
    .await?;
    for row in &heads {
        let profile: Uuid = row.try_get("profile_id")?;
        let revision: i64 = row.try_get("revision")?;
        match copy_head_one(pool, profile, revision).await {
            Ok(moved) => report.heads_copied += u64::from(moved),
            Err(e) => {
                report.failed += 1;
                tracing::warn!(%profile, revision, error = %e, "progression.nightly.head_failed");
            }
        }
    }
    Ok(report)
}

async fn summarize_one(pool: &PgPool, session: Uuid, profile: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let built = insert_summary(&mut tx, session).await?;
    refresh_profile(&mut tx, profile).await?;
    tx.commit().await?;
    Ok(built)
}

async fn recount_one(
    pool: &PgPool,
    profile: Uuid,
    watermark: OffsetDateTime,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    refresh_profile(&mut tx, profile).await?;
    sqlx::query(
        "UPDATE profile_progress SET consolidated_through = $2 \
         WHERE profile_id = $1 AND (consolidated_through IS NULL OR consolidated_through < $2)",
    )
    .bind(profile)
    .bind(watermark)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

async fn copy_head_one(pool: &PgPool, profile: Uuid, revision: i64) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO profile_progress (profile_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(profile)
        .execute(&mut *tx)
        .await?;
    let moved = copy_save_numbers(&mut tx, profile, revision).await?;
    tx.commit().await?;
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::is_race_fact;
    use serde_json::json;

    fn map(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn race_facts_are_race_board_scores_and_race_results() {
        let score =
            |board: &str, accepted: bool| map(json!({"board": board, "accepted": accepted}));
        assert!(is_race_fact("score", &score("1:1", true)));
        assert!(is_race_fact(
            "score",
            &map(json!({"data": {"board": "1:1", "accepted": true}}))
        ));
        assert!(!is_race_fact("score", &score("1:1", false)));
        for board in ["4:10", "6:1", "10:1", "11:1", "1", ""] {
            assert!(!is_race_fact("score", &score(board, true)), "{board}");
        }
        assert!(!is_race_fact("score", &map(json!({"board": "1:1"}))));
        assert!(is_race_fact("race_result", &map(json!({}))));
        assert!(!is_race_fact(
            "xp",
            &map(json!({"board": "1:1", "accepted": true}))
        ));
    }
}
