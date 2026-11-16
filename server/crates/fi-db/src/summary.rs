//! The nightly server summary (owner-decisions, OD-122 follow-up 2026-10-26 "a nightly server
//! summary"; CORE_API.md §2.4): `fi-cli consolidate-nightly` computes [`NightlySummary`] of every
//! game from the game tables ([`compute`]) and stores it as the game's `summary` gold snapshot
//! ([`store`]); core's `GET /core/v1/summary` reads it back ([`latest_in`]) beside the live
//! `onlineNow`. Aggregates only: no id, name or per-player value leaves this module.
//!
//! **Windows.** "Day" figures cover the UTC day `[day, day + 1)`; every "total" counts what
//! happened before `day + 1` (rows dated by the server: `admitted_at`, `opened_at`, `used_at`,
//! `received_at`, `committed_at`, `created_at`), so a rerun for the same day after it ended gives
//! the same numbers and the stored row is replaced, never duplicated (`gold_snapshot` is keyed by
//! game and name). Active week and month are the 7 and 30 days ending with the day.
//!
//! | Figure | Computed from |
//! | --- | --- |
//! | `players.accounts` | distinct non-deleted accounts owning a non-deleted profile of the game created before the end |
//! | `players.profiles` | non-deleted profiles of the game created before the end |
//! | `players.active*` | distinct profiles with an admitted, non-dedicated `play_session` overlapping the window |
//! | `time.playSeconds*` | Σ (end or the day's end) − admission over those sessions, clipped to the window |
//! | `online.worldsHosted*` | `world` rows by `opened_at` |
//! | `online.joins*` | `mm_grant` rows by `used_at` (the host reported the peer) |
//! | `online.sessions*` | admitted, non-dedicated sessions by `admitted_at` |
//! | `online.races*` | race facts (`progression_event`, `fi_db::progression::race_fact`: accepted `score` facts on `scoreboardType` 1, the game's SubmitScore of a finished race, plus accepted `race_result` facts) by `received_at` |
//! | `progression.creditsEarned`, `xpTotal`, `levelsTotal` | Σ `winnings` (TotalWinnings), `xp`, `level` of each non-deleted profile's newest `save_revision` committed before the end (range-checked, an unreadable number counts as unknown) |
//! | `progression.xpDay` | Σ `session_summary.xp_gained` of sessions that ended within the day |
//!
//! Not published because nothing stored supports it: distance (the hook's odometer is kept only
//! as the idle rule's progress baseline, `play_session.activity_odo_base_m`, reset on progress and
//! on a hook restart), cars (the save summary's `cars` is not read yet) and purchases or levels
//! gained (no fact records them).
//!
//! Runs as a `fi_maintenance` login (SELECT on every table read here; the snapshot write); core
//! reads the snapshot as `fi_runtime`.

use crate::identity::{IdentityError, ms};
use crate::progression::{I64_MAX, clamp_bigint, number_within, race_fact};
use fi_protocol::core::{
    NightlySummary, SummaryOnline, SummaryPlayers, SummaryProgression, SummaryTime,
};
use sqlx::{PgConnection, PgPool, Row};
use time::{Date, Duration, OffsetDateTime, Time};

/// The `gold_snapshot.name` of the summary.
pub const SNAPSHOT: &str = "summary";

/// Midnight UTC of `day`.
fn midnight(day: Date) -> OffsetDateTime {
    day.with_time(Time::MIDNIGHT).assume_utc()
}

/// The UTC day before `now`'s: what a nightly run summarizes.
pub fn yesterday(now: OffsetDateTime) -> Date {
    now.to_offset(time::UtcOffset::UTC)
        .date()
        .previous_day()
        .unwrap_or(Date::MIN)
}

/// The day a nightly run started now summarizes: yesterday, UTC.
pub fn last_day() -> Date {
    yesterday(OffsetDateTime::now_utc())
}

/// The summary of `game` for the UTC day `day` (module docs).
pub async fn compute(db: &PgPool, game: &str, day: Date) -> Result<NightlySummary, IdentityError> {
    let start = midnight(day);
    let end = start + Duration::DAY;
    let week = end - Duration::days(7);
    let month = end - Duration::days(30);
    let n = |path: &str| number_within(&format!("h.summary->'{path}'"), "0", I64_MAX);
    let sum = |path: &str| clamp_bigint(&format!("(SELECT sum({}) FROM head h)", n(path)));
    let sql = format!(
        "WITH prof AS ( \
           SELECT id, account_id FROM profile \
           WHERE game = $1 AND status <> 'deleted' AND created_at < $3), \
         sess AS ( \
           SELECT s.profile_id, s.admitted_at, \
             LEAST(COALESCE(s.ended_at, 'infinity'::timestamptz), $3) AS until \
           FROM play_session s JOIN profile p ON p.id = s.profile_id \
           WHERE p.game = $1 AND s.admitted_at IS NOT NULL AND s.admitted_at < $3 \
             AND NOT s.dedicated), \
         head AS ( \
           SELECT DISTINCT ON (r.profile_id) r.summary \
           FROM save_revision r JOIN prof ON prof.id = r.profile_id \
           WHERE r.committed_at < $3 \
           ORDER BY r.profile_id, r.revision DESC) \
         SELECT \
           (SELECT count(DISTINCT prof.account_id) FROM prof \
              JOIN account a ON a.id = prof.account_id WHERE a.status <> 'deleted') AS accounts, \
           (SELECT count(*) FROM prof) AS profiles, \
           (SELECT count(DISTINCT profile_id) FROM sess WHERE until > $2) AS active_day, \
           (SELECT count(DISTINCT profile_id) FROM sess WHERE until > $4) AS active_week, \
           (SELECT count(DISTINCT profile_id) FROM sess WHERE until > $5) AS active_month, \
           (SELECT COALESCE(floor(sum(GREATEST(extract(epoch FROM until - admitted_at), 0))), 0) \
              FROM sess)::bigint AS play_total, \
           (SELECT COALESCE(floor(sum(GREATEST(extract(epoch FROM until - GREATEST(admitted_at, $2)), 0))), 0) \
              FROM sess WHERE until > $2)::bigint AS play_day, \
           (SELECT count(*) FROM world WHERE game = $1 AND opened_at < $3) AS worlds, \
           (SELECT count(*) FROM world WHERE game = $1 AND opened_at >= $2 AND opened_at < $3) AS worlds_day, \
           (SELECT count(*) FROM mm_grant WHERE game = $1 AND used_at < $3) AS joins, \
           (SELECT count(*) FROM mm_grant WHERE game = $1 AND used_at >= $2 AND used_at < $3) AS joins_day, \
           (SELECT count(*) FROM sess) AS sessions, \
           (SELECT count(*) FROM sess WHERE admitted_at >= $2) AS sessions_day, \
           (SELECT count(*) FILTER (WHERE e.received_at >= $2) FROM progression_event e \
              JOIN profile p ON p.id = e.profile_id \
              WHERE p.game = $1 AND {race} AND e.received_at < $3) AS races_day, \
           (SELECT count(*) FROM progression_event e \
              JOIN profile p ON p.id = e.profile_id \
              WHERE p.game = $1 AND {race} AND e.received_at < $3) AS races, \
           {credits} AS credits, {xp} AS xp, {levels} AS levels, \
           {xp_day} AS xp_day, \
           clock_timestamp() AS built_at",
        race = race_fact(),
        credits = sum("winnings"),
        xp = sum("xp"),
        levels = sum("level"),
        xp_day = clamp_bigint(
            "(SELECT sum(GREATEST(m.xp_gained, 0)) FROM session_summary m \
               JOIN profile p ON p.id = m.profile_id \
               WHERE p.game = $1 AND m.ended_at >= $2 AND m.ended_at < $3)"
        ),
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(game)
        .bind(start)
        .bind(end)
        .bind(week)
        .bind(month)
        .fetch_one(db)
        .await?;
    let c = |name: &str| -> Result<u64, sqlx::Error> {
        Ok(row.try_get::<Option<i64>, _>(name)?.unwrap_or(0).max(0) as u64)
    };
    let built_at: OffsetDateTime = row.try_get("built_at")?;
    Ok(NightlySummary {
        built_at: fi_protocol::instant::format_unix_ms(ms(built_at).millis()).unwrap_or_default(),
        day: format!(
            "{:04}-{:02}-{:02}",
            day.year(),
            u8::from(day.month()),
            day.day()
        ),
        players: SummaryPlayers {
            accounts: c("accounts")?,
            profiles: c("profiles")?,
            active_day: c("active_day")?,
            active_week: c("active_week")?,
            active_month: c("active_month")?,
        },
        time: SummaryTime {
            play_seconds_total: c("play_total")?,
            play_seconds_day: c("play_day")?,
        },
        online: SummaryOnline {
            worlds_hosted: c("worlds")?,
            worlds_hosted_day: c("worlds_day")?,
            joins: c("joins")?,
            joins_day: c("joins_day")?,
            sessions_total: c("sessions")?,
            sessions_day: c("sessions_day")?,
            races: c("races")?,
            races_day: c("races_day")?,
        },
        progression: SummaryProgression {
            credits_earned: c("credits")?,
            xp_total: c("xp")?,
            xp_day: c("xp_day")?,
            levels_total: c("levels")?,
        },
    })
}

/// Stores `summary` as `game`'s `summary` gold snapshot (replacing the previous one).
pub async fn store(db: &PgPool, game: &str, summary: &NightlySummary) -> Result<(), IdentityError> {
    let payload = serde_json::to_value(summary)
        .map_err(|e| IdentityError::Db(sqlx::Error::Decode(Box::new(e))))?;
    crate::universe::put_gold_snapshot(db, game, SNAPSHOT, &payload).await
}

/// The stored summary of `game`; `None` before the first nightly run, or when the stored row
/// does not read as this build's [`NightlySummary`] (logged; the next run replaces it).
pub async fn latest_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Option<NightlySummary>, IdentityError> {
    let payload: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT payload FROM gold_snapshot WHERE game = $1 AND name = $2")
            .bind(game)
            .bind(SNAPSHOT)
            .fetch_optional(conn)
            .await?;
    Ok(payload.and_then(|p| match serde_json::from_value(p) {
        Ok(s) => Some(s),
        Err(_) => {
            tracing::warn!(game, "summary.snapshot_unreadable");
            None
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Month, UtcOffset};

    fn date(d: u8) -> Date {
        Date::from_calendar_date(2026, Month::October, d).unwrap()
    }

    #[test]
    fn yesterday_is_the_previous_utc_day() {
        let at = |d: u8, h: u8, offset: i8| {
            date(d)
                .with_hms(h, 30, 0)
                .unwrap()
                .assume_offset(UtcOffset::from_hms(offset, 0, 0).unwrap())
        };
        assert_eq!(yesterday(at(26, 0, 0)), date(25));
        // 23:30 at UTC-3 is already the 27th in UTC.
        assert_eq!(yesterday(at(26, 23, -3)), date(26));
        assert_eq!(midnight(date(25)) + Duration::DAY, midnight(date(26)));
    }
}
