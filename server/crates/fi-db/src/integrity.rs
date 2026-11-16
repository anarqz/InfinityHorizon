//! The RPC gates' records (SER-M3 step 6, OD-48; GAME_PLATFORM.md §3a "Supporter universe"):
//! strikes of refused reports ([`strike_rpc_in`], `integrity_strike`), the baseline of what was
//! last accepted of each profile's sync (`integrity_baseline`, migrations
//! `20261019120200_integrity.sql`, `20261019130200_integrity_episode.sql` and
//! `20261019194500_guard.sql` and `20261019230000_guard_fixes.sql`: the numbers, the guardrails'
//! fast and slow rates at them, the level high-water mark, the open refusal episode of each half
//! and the facts a refusal by score judged in it), which `fi_gamedb::gates` measures the next sync against, and the
//! guardrails' assessments of every Hardcore sync and save commit (`integrity_assessment`, OD-62:
//! [`record_assessment_in`], [`assessments_in`]).
//!
//! Every function takes the request's connection (`*_in`, its one transaction) and the baseline's
//! times are the database clock ([`Baselines::now`] is read with it), so the gates never compare
//! two clocks. A baseline is per profile **and mode**: a profile whose mode changed starts a new one.
//!
//! - [`lock_in`]: the transaction's lock on a profile's baseline, taken before it is read, so
//!   concurrent syncs of a profile are measured one after the other (a row lock would lock nothing
//!   before the first sync);
//! - [`baselines_in`]: both halves of the baseline (UpdateAll's numbers, UpdateAllTables' garage
//!   size, each with its rates) and the database clock, in one statement;
//! - [`set_profile_baseline_in`], [`set_garage_baseline_in`]: an accepted sync seeds or moves its
//!   half and ends its refusal episode (upserts: the other half is kept);
//! - [`open_episode_in`], [`close_episode_in`]: a refused sync opens its half's episode (the
//!   caller strikes only when this one opened it); an accepted one that moves nothing closes it;
//! - [`judge_in`]: a sync refused by score keeps the fact checks' values it judged with the
//!   profile half's episode, so the same value is not refused again (CLOUD_SAVES.md §7.1 rule 4);
//! - [`garage_size_after_in`]: the size the stored garage would have after an upload;
//! - [`garage_car_ids_in`]: the stored garage's car ids (the top-speed signal's reference);
//! - [`record_assessment_in`]: one guardrails assessment (accepted or refused), on the request's
//!   transaction for a sync, after the commit or the refusal for a save;
//! - [`assessments_in`]: the moderator's read (CORE_API.md §5), newest first;
//! - [`audit_due_in`], [`record_audit_in`]: the save audit job's queue (accepted Hardcore
//!   revisions without an audit) and its shadow record (source `save_audit`, OD-67, ADR 0021;
//!   migration `20261020100000_save_audit.sql`), one per revision.

use crate::identity::{IdentityError, ms};
use fi_domain::UnixMs;
use fi_guard::{Assessment, Floor, Judged, Rates};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The sources an RPC gate strikes with (`integrity_strike.source`, migration
/// `20261019115900_rpc_strikes.sql`).
pub const RPC_SOURCES: [&str; 5] = [
    "profile_gate",
    "race_time",
    "top_speed",
    "xp_rate",
    "garage",
];

/// The `game_profile_table` name of the garage.
pub const GARAGE_TABLE: &str = "garageCars";

/// Strikes `profile_id` for a report an RPC gate refused; `reason` is English, without player
/// data, cut to the column's 500 characters. Returns the strike's id. A source outside
/// [`RPC_SOURCES`] is refused by the table's CHECK.
pub async fn strike_rpc_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
    source: &str,
    reason: &str,
) -> Result<Uuid, IdentityError> {
    let reason: String = reason.chars().filter(|c| *c != '\0').take(500).collect();
    let reason = if reason.is_empty() {
        source.to_owned()
    } else {
        reason
    };
    Ok(sqlx::query_scalar(
        "INSERT INTO integrity_strike (profile_id, source, reason) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(profile_id)
    .bind(source)
    .bind(reason)
    .fetch_one(&mut *conn)
    .await?)
}

/// Whose baseline: a profile of a game, in the mode it plays (`hardcore` or `noob`, the table's
/// CHECK).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subject<'a> {
    /// The game (`fh2`).
    pub game: &'a str,
    /// The profile.
    pub profile_id: Uuid,
    /// `hardcore` or `noob`.
    pub mode: &'a str,
}

/// The numbers of an accepted UpdateAll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProfileNumbers {
    /// Credits.
    pub credits: i64,
    /// XP.
    pub xp: i64,
    /// Level.
    pub level: i64,
    /// Total race winnings.
    pub winnings: i64,
    /// The top speed record, m/s (finite, not negative).
    pub top_speed_mps: f64,
}

/// The last accepted UpdateAll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProfileBaseline {
    /// Its numbers.
    pub numbers: ProfileNumbers,
    /// When it was accepted (database clock): the rates' time.
    pub synced_at: UnixMs,
    /// The guardrails' rates per hour then (`cars` is the garage half's, always 0 here; a NULL
    /// column, a seeded half or a row from before the rates, reads as 0).
    pub rates: Rates,
    /// The highest level accepted since the seed (`None`: `numbers.level`).
    pub level_high: Option<i64>,
    /// The fact checks' values refused by score since it was accepted.
    pub judged: Judged,
}

/// The last accepted UpdateAllTables that carried a garage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GarageBaseline {
    /// The garage's size after it.
    pub cars: i64,
    /// When it was accepted (database clock): the rate's time.
    pub synced_at: UnixMs,
    /// Cars per hour then (0: none recently).
    pub rate: f64,
}

/// A profile's baseline in one mode, each half `None` until its first accepted sync seeds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Baselines {
    /// UpdateAll's half.
    pub profile: Option<ProfileBaseline>,
    /// UpdateAllTables' half.
    pub garage: Option<GarageBaseline>,
    /// The database clock of the read: the gates' "now".
    pub now: UnixMs,
}

/// Locks `subject`'s baselines, every mode, until the transaction ends (a transaction-scoped
/// advisory lock on the game and profile): a concurrent sync of the same profile waits here, then
/// reads what this one wrote. Exists before the first sync, unlike a row. Take it on the request's
/// transaction itself, not inside a savepoint (released only at the end of the transaction either
/// way), before reading the baseline.
pub async fn lock_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('fi.integrity_baseline'), \
           hashtext($1 || ':' || $2::text))",
    )
    .bind(game)
    .bind(profile_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The baseline of `subject` and the database clock.
pub async fn baselines_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
) -> Result<Baselines, IdentityError> {
    let r = sqlx::query(
        "SELECT clock_timestamp() AS now, b.credits, b.xp, b.level, b.winnings, b.top_speed_mps, \
           b.synced_at, b.garage_cars, b.tables_synced_at, b.rate_credits, b.rate_unexplained, \
           b.rate_winnings, b.rate_xp, b.rate_levels, b.rate_cars, b.rate_day_unexplained, \
           b.rate_day_winnings, b.rate_day_xp, b.rate_day_levels, b.level_high, \
           b.judged_level_deviation, b.judged_drop_xp, b.judged_drop_level \
         FROM (SELECT 1) AS one \
         LEFT JOIN integrity_baseline b ON b.game = $1 AND b.profile_id = $2 AND b.mode = $3",
    )
    .bind(subject.game)
    .bind(subject.profile_id)
    .bind(subject.mode)
    .fetch_one(&mut *conn)
    .await?;
    let rate = |name: &str| -> Result<f64, sqlx::Error> {
        Ok(r.try_get::<Option<f64>, _>(name)?.unwrap_or(0.0))
    };
    let synced: Option<OffsetDateTime> = r.try_get("synced_at")?;
    let profile = match synced {
        Some(at) => Some(ProfileBaseline {
            numbers: ProfileNumbers {
                credits: r.try_get("credits")?,
                xp: r.try_get("xp")?,
                level: r.try_get("level")?,
                winnings: r.try_get("winnings")?,
                top_speed_mps: r.try_get("top_speed_mps")?,
            },
            synced_at: ms(at),
            rates: Rates {
                credits: rate("rate_credits")?,
                unexplained: rate("rate_unexplained")?,
                winnings: rate("rate_winnings")?,
                xp: rate("rate_xp")?,
                levels: rate("rate_levels")?,
                cars: 0.0,
                day_unexplained: rate("rate_day_unexplained")?,
                day_winnings: rate("rate_day_winnings")?,
                day_xp: rate("rate_day_xp")?,
                day_levels: rate("rate_day_levels")?,
            }
            .sanitized(),
            level_high: r.try_get("level_high")?,
            judged: Judged {
                level_deviation: r
                    .try_get::<Option<i64>, _>("judged_level_deviation")?
                    .unwrap_or(0),
                token_deviation: 0,
                drop_floor: match (
                    r.try_get::<Option<i64>, _>("judged_drop_xp")?,
                    r.try_get::<Option<i64>, _>("judged_drop_level")?,
                ) {
                    (Some(xp), Some(level)) => Some(Floor { xp, level }),
                    _ => None,
                },
            },
        }),
        None => None,
    };
    let tables: Option<OffsetDateTime> = r.try_get("tables_synced_at")?;
    let garage = match tables {
        Some(at) => Some(GarageBaseline {
            cars: i64::from(r.try_get::<i32, _>("garage_cars")?),
            synced_at: ms(at),
            rate: Rates {
                cars: rate("rate_cars")?,
                ..Rates::default()
            }
            .sanitized()
            .cars,
        }),
        None => None,
    };
    Ok(Baselines {
        profile,
        garage,
        now: ms(r.try_get::<OffsetDateTime, _>("now")?),
    })
}

/// An accepted UpdateAll seeds or moves the profile half of `subject`'s baseline: its numbers,
/// the guardrails' rates after it (all but `cars`) and the level high-water mark after it
/// (`level_high`; `None`: the numbers' level), now on the database clock, and ends the half's
/// refusal episode, if one is open, with the facts it judged; the garage half is kept.
pub async fn set_profile_baseline_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    numbers: &ProfileNumbers,
    rates: &Rates,
    level_high: Option<i64>,
) -> Result<(), IdentityError> {
    let r = rates.sanitized();
    let level_high = level_high.map(|h| h.max(numbers.level).max(0));
    sqlx::query(
        "INSERT INTO integrity_baseline \
           (game, profile_id, mode, credits, xp, level, winnings, top_speed_mps, synced_at, \
            rate_credits, rate_unexplained, rate_winnings, rate_xp, rate_levels, \
            rate_day_unexplained, rate_day_winnings, rate_day_xp, rate_day_levels, level_high) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, clock_timestamp(), $9, $10, $11, $12, $13, \
           $14, $15, $16, $17, $18) \
         ON CONFLICT (game, profile_id, mode) DO UPDATE SET \
           credits = EXCLUDED.credits, xp = EXCLUDED.xp, level = EXCLUDED.level, \
           winnings = EXCLUDED.winnings, top_speed_mps = EXCLUDED.top_speed_mps, \
           synced_at = EXCLUDED.synced_at, rate_credits = EXCLUDED.rate_credits, \
           rate_unexplained = EXCLUDED.rate_unexplained, rate_winnings = EXCLUDED.rate_winnings, \
           rate_xp = EXCLUDED.rate_xp, rate_levels = EXCLUDED.rate_levels, \
           rate_day_unexplained = EXCLUDED.rate_day_unexplained, \
           rate_day_winnings = EXCLUDED.rate_day_winnings, rate_day_xp = EXCLUDED.rate_day_xp, \
           rate_day_levels = EXCLUDED.rate_day_levels, level_high = EXCLUDED.level_high, \
           profile_refused_at = NULL, judged_level_deviation = NULL, judged_drop_xp = NULL, \
           judged_drop_level = NULL, updated_at = clock_timestamp()",
    )
    .bind(subject.game)
    .bind(subject.profile_id)
    .bind(subject.mode)
    .bind(numbers.credits)
    .bind(numbers.xp)
    .bind(numbers.level)
    .bind(numbers.winnings)
    .bind(numbers.top_speed_mps)
    .bind(r.credits)
    .bind(r.unexplained)
    .bind(r.winnings)
    .bind(r.xp)
    .bind(r.levels)
    .bind(r.day_unexplained)
    .bind(r.day_winnings)
    .bind(r.day_xp)
    .bind(r.day_levels)
    .bind(level_high)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A profile sync refused by score (`anomaly_score`) keeps the fact checks' values it judged
/// (`judged`, the memory before it merged with the assessment's: [`Assessment::judged_after_refusal`])
/// with the profile half's open refusal episode, so the next sync measured against the same
/// accepted state does not score them again (CLOUD_SAVES.md §7.1 rule 4). The next accepted sync
/// clears them ([`set_profile_baseline_in`], [`close_episode_in`]). A profile without a baseline
/// row keeps nothing. The caller holds [`lock_in`].
pub async fn judge_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    judged: &Judged,
) -> Result<(), IdentityError> {
    let positive = |v: i64| (v > 0).then_some(v);
    sqlx::query(
        "UPDATE integrity_baseline SET judged_level_deviation = $4, judged_drop_xp = $5, \
           judged_drop_level = $6, updated_at = clock_timestamp() \
         WHERE game = $1 AND profile_id = $2 AND mode = $3",
    )
    .bind(subject.game)
    .bind(subject.profile_id)
    .bind(subject.mode)
    .bind(positive(judged.level_deviation))
    .bind(judged.drop_floor.map(|f| f.xp.max(0)))
    .bind(judged.drop_floor.map(|f| f.level.max(0)))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// An accepted UpdateAllTables that carried a garage seeds or moves the garage half of
/// `subject`'s baseline: the garage's size after it and the cars rate after it, now on the
/// database clock, and ends the half's refusal episode, if one is open; the profile half is kept.
pub async fn set_garage_baseline_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    cars: i64,
    rate: f64,
) -> Result<(), IdentityError> {
    let int = |v: i64| i32::try_from(v.max(0)).unwrap_or(i32::MAX);
    let rate = Rates {
        cars: rate,
        ..Rates::default()
    }
    .sanitized()
    .cars;
    sqlx::query(
        "INSERT INTO integrity_baseline \
           (game, profile_id, mode, garage_cars, tables_synced_at, rate_cars) \
         VALUES ($1, $2, $3, $4, clock_timestamp(), $5) \
         ON CONFLICT (game, profile_id, mode) DO UPDATE SET \
           garage_cars = EXCLUDED.garage_cars, tables_synced_at = EXCLUDED.tables_synced_at, \
           rate_cars = EXCLUDED.rate_cars, garage_refused_at = NULL, \
           updated_at = clock_timestamp()",
    )
    .bind(subject.game)
    .bind(subject.profile_id)
    .bind(subject.mode)
    .bind(int(cars))
    .bind(rate)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A half of the baseline, which has its own refusal episodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    /// UpdateAll's numbers (`profile_refused_at`).
    Profile,
    /// UpdateAllTables' garage (`garage_refused_at`).
    Garage,
}

/// A refused sync of `half`: opens the half's refusal episode unless one is open, and returns
/// whether this call opened it, which is when the caller strikes (one strike per episode, migration
/// `20261019130200_integrity_episode.sql`). Creates the row when the profile has no baseline yet
/// (both halves stay NULL). The caller holds [`lock_in`].
pub async fn open_episode_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    half: Half,
) -> Result<bool, IdentityError> {
    let sql = match half {
        Half::Profile => {
            "INSERT INTO integrity_baseline (game, profile_id, mode, profile_refused_at) \
             VALUES ($1, $2, $3, clock_timestamp()) \
             ON CONFLICT (game, profile_id, mode) DO UPDATE SET \
               profile_refused_at = EXCLUDED.profile_refused_at, updated_at = clock_timestamp() \
             WHERE integrity_baseline.profile_refused_at IS NULL \
             RETURNING true"
        }
        Half::Garage => {
            "INSERT INTO integrity_baseline (game, profile_id, mode, garage_refused_at) \
             VALUES ($1, $2, $3, clock_timestamp()) \
             ON CONFLICT (game, profile_id, mode) DO UPDATE SET \
               garage_refused_at = EXCLUDED.garage_refused_at, updated_at = clock_timestamp() \
             WHERE integrity_baseline.garage_refused_at IS NULL \
             RETURNING true"
        }
    };
    let opened: Option<bool> = sqlx::query_scalar(sql)
        .bind(subject.game)
        .bind(subject.profile_id)
        .bind(subject.mode)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(opened.is_some())
}

/// An accepted sync of `half` that moves no baseline (an UpdateAll without both sections, before
/// any baseline) still ends the half's open refusal episode. The setters end it themselves.
pub async fn close_episode_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    half: Half,
) -> Result<(), IdentityError> {
    let sql = match half {
        Half::Profile => {
            "UPDATE integrity_baseline SET profile_refused_at = NULL, \
               judged_level_deviation = NULL, judged_drop_xp = NULL, judged_drop_level = NULL, \
               updated_at = clock_timestamp() \
             WHERE game = $1 AND profile_id = $2 AND mode = $3 AND profile_refused_at IS NOT NULL"
        }
        Half::Garage => {
            "UPDATE integrity_baseline SET garage_refused_at = NULL, updated_at = clock_timestamp() \
             WHERE game = $1 AND profile_id = $2 AND mode = $3 AND garage_refused_at IS NOT NULL"
        }
    };
    sqlx::query(sql)
        .bind(subject.game)
        .bind(subject.profile_id)
        .bind(subject.mode)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// How many cars the stored garage of `profile_id` would hold after an upload of `rows` (row keys)
/// and `deletes` (keys removed after the rows are merged, as `gameplane::apply_table_in` does):
/// the stored keys and the new ones, less the deleted. Locks the stored garage row, when there is
/// one, until the transaction ends; [`lock_in`] is what orders concurrent uploads of a profile
/// before its first garage exists.
pub async fn garage_size_after_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    rows: &[String],
    deletes: &[String],
) -> Result<i64, IdentityError> {
    let clean =
        |keys: &[String]| -> Vec<String> { keys.iter().map(|k| k.replace('\0', "")).collect() };
    Ok(sqlx::query_scalar(
        "WITH stored AS ( \
           SELECT data FROM game_profile_table WHERE game = $1 AND profile_id = $2 AND name = $3 \
           FOR UPDATE) \
         SELECT count(*) FROM ( \
           SELECT jsonb_object_keys(data) AS k FROM stored \
           UNION SELECT unnest($4::text[])) AS after \
         WHERE NOT (k = ANY($5::text[]))",
    )
    .bind(game)
    .bind(profile_id)
    .bind(GARAGE_TABLE)
    .bind(clean(rows))
    .bind(clean(deletes))
    .fetch_one(&mut *conn)
    .await?)
}

/// The car ids of the stored garage of `profile_id`, ascending and distinct (rows whose `carId`
/// is not an integer are left out).
pub async fn garage_car_ids_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
) -> Result<Vec<i64>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT (e.value ->> 'carId')::bigint AS car \
         FROM game_profile_table t, jsonb_each(t.data) AS e \
         WHERE t.game = $1 AND t.profile_id = $2 AND t.name = $3 \
           AND jsonb_typeof(e.value -> 'carId') = 'number' \
           AND (e.value ->> 'carId') ~ '^-?[0-9]{1,18}$' \
         ORDER BY car",
    )
    .bind(game)
    .bind(profile_id)
    .bind(GARAGE_TABLE)
    .fetch_all(&mut *conn)
    .await?)
}

/// The car of each stored garage row of `profile_id` among `garage_ids` (a row's key is its
/// garage id; rows whose `carId` is not an integer are left out): what an UpdateAllTables'
/// `purchasedParts` rows name through `garageId` (OD-178).
pub async fn garage_cars_of_in(
    conn: &mut PgConnection,
    game: &str,
    profile_id: Uuid,
    garage_ids: &[String],
) -> Result<Vec<(String, i64)>, IdentityError> {
    if garage_ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(sqlx::query_as(
        "SELECT e.key, (e.value ->> 'carId')::bigint \
         FROM game_profile_table t, jsonb_each(t.data) AS e \
         WHERE t.game = $1 AND t.profile_id = $2 AND t.name = $3 AND e.key = ANY($4) \
           AND jsonb_typeof(e.value -> 'carId') = 'number' \
           AND (e.value ->> 'carId') ~ '^-?[0-9]{1,18}$'",
    )
    .bind(game)
    .bind(profile_id)
    .bind(GARAGE_TABLE)
    .bind(garage_ids)
    .fetch_all(&mut *conn)
    .await?)
}

/// What an assessment measured (`integrity_assessment.source`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssessmentSource {
    /// A save commit (`PUT .../save`).
    SaveCommit,
    /// `OnlineProfile.UpdateAll`.
    ProfileSync,
    /// `OnlineProfile.UpdateAllTables` carrying a garage.
    TablesSync,
    /// The save audit job's read of a revision's PlayerDatabase.
    SaveAudit,
    /// The nightly guardrails pass's assessment of a committed revision (hybrid Hardcore checks,
    /// CLOUD_SAVES.md §7.5; [`crate::guard_nightly`]).
    SaveNightly,
}

impl AssessmentSource {
    /// The stored value.
    pub const fn as_str(self) -> &'static str {
        match self {
            AssessmentSource::SaveCommit => "save_commit",
            AssessmentSource::ProfileSync => "profile_sync",
            AssessmentSource::TablesSync => "tables_sync",
            AssessmentSource::SaveAudit => "save_audit",
            AssessmentSource::SaveNightly => "save_nightly",
        }
    }
}

/// The `deltas` document of an assessment (camelCase, null where a side lacks the number).
pub fn deltas_json(a: &Assessment) -> Value {
    let d = &a.deltas;
    json!({
        "credits": d.credits,
        "xp": d.xp,
        "levels": d.levels,
        "winnings": d.winnings,
        "cars": d.cars,
        "unexplained": d.unexplained,
        "game": a.game.map(|g| json!({
            "credits": g.credits,
            "xp": g.xp,
            "levels": g.levels,
            "hoursSince": g.hours_since,
        })),
    })
}

/// The `rates` document of an assessment: per hour after it.
pub fn rates_json(a: &Assessment) -> Value {
    let r = a.rates.sanitized();
    json!({
        "credits": r.credits,
        "unexplained": r.unexplained,
        "winnings": r.winnings,
        "xp": r.xp,
        "levels": r.levels,
        "cars": r.cars,
        "dayUnexplained": r.day_unexplained,
        "dayWinnings": r.day_winnings,
        "dayXp": r.day_xp,
        "dayLevels": r.day_levels,
    })
}

/// Records one guardrails assessment of `subject` in its play session `session_id`: `refusal`
/// is the refusing rule's code (a ceiling's, `ANOMALY_SCORE`), `None` when accepted. Returns the
/// row's id. The runtime may insert and read these rows, never change them.
pub async fn record_assessment_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    session_id: Uuid,
    source: AssessmentSource,
    assessment: &Assessment,
    refusal: Option<&str>,
) -> Result<Uuid, IdentityError> {
    let reasons: Vec<&str> = assessment.reasons.iter().map(|r| r.code()).collect();
    Ok(sqlx::query_scalar(
        "INSERT INTO integrity_assessment \
           (game, profile_id, mode, play_session, source, hours, deltas, rates, score, reasons, \
            refusal, decision, would_block, rules_version) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) RETURNING id",
    )
    .bind(subject.game)
    .bind(subject.profile_id)
    .bind(subject.mode)
    .bind(session_id)
    .bind(source.as_str())
    .bind(assessment.hours.clamp(0.0, fi_guard::MAX_HOURS))
    .bind(deltas_json(assessment))
    .bind(rates_json(assessment))
    .bind(i16::from(assessment.score.min(100)))
    .bind(&reasons)
    .bind(refusal)
    .bind(if refusal.is_some() {
        "refused"
    } else {
        "accepted"
    })
    .bind(assessment.would_block())
    .bind(fi_guard::RULES_VERSION)
    .fetch_one(&mut *conn)
    .await?)
}

/// Whose assessments a moderator reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssessedBy {
    /// Every profile of the game (the moderation queue).
    Everyone,
    /// The profile of the game with this XUID.
    Xuid(i64),
    /// Every profile of the game of this account.
    Account(Uuid),
}

/// One stored assessment, with its profile's XUID (never an e-mail, an IP or a gamertag).
#[derive(Clone, Debug, PartialEq)]
pub struct AssessmentRow {
    /// Its id (the page cursor).
    pub id: Uuid,
    /// When it was recorded.
    pub at: UnixMs,
    /// The game.
    pub game: String,
    /// The profile's XUID.
    pub xuid: i64,
    /// The profile's mode then.
    pub mode: String,
    /// `save_commit`, `profile_sync`, `tables_sync`, `save_audit` or `save_nightly`.
    pub source: String,
    /// The play session.
    pub session_id: Uuid,
    /// Hours measured.
    pub hours: f64,
    /// [`deltas_json`].
    pub deltas: Value,
    /// [`rates_json`].
    pub rates: Value,
    /// 0-100.
    pub score: i16,
    /// Reason codes.
    pub reasons: Vec<String>,
    /// The refusing rule's code, if refused.
    pub refusal: Option<String>,
    /// `accepted`, `refused`, or `flagged` (the nightly pass found what the inline guardrails
    /// would have refused; nothing was refused or rolled back).
    pub decision: String,
    /// Score 60 or more.
    pub would_block: bool,
    /// `fi_guard::RULES_VERSION` then.
    pub rules_version: String,
    /// The revision a save audit read or the nightly pass assessed (`None` for the other
    /// sources).
    pub revision: Option<i64>,
    /// The save audit's numbers (`fi_gamedb::audit::Facts`; `None` for the other sources).
    pub audit: Option<Value>,
    /// A seed check's bundle SHA-256 (CR-S52; `None` for the other sources).
    pub bundle_sha256: Option<Vec<u8>>,
    /// A seed check's target: the XUID of the Hardcore profile it would seed.
    pub seed_target: Option<i64>,
}

/// The assessments of `game` for `who` with a score of at least `min_score`, newest first, at
/// most `limit` (1 to 200), older than the cursor `before` (an id of the previous page) when
/// given.
pub async fn assessments_in(
    conn: &mut PgConnection,
    game: &str,
    who: AssessedBy,
    min_score: i16,
    before: Option<Uuid>,
    limit: i64,
) -> Result<Vec<AssessmentRow>, IdentityError> {
    let (xuid, account) = match who {
        AssessedBy::Everyone => (None, None),
        AssessedBy::Xuid(x) => (Some(x), None),
        AssessedBy::Account(a) => (None, Some(a)),
    };
    let rows = sqlx::query(
        "SELECT a.id, a.at, a.game, p.xuid, a.mode, a.source, a.play_session, a.hours, a.deltas, \
           a.rates, a.score, a.reasons, a.refusal, a.decision, a.would_block, a.rules_version, \
           a.revision, a.audit, a.bundle_sha256, t.xuid AS seed_target \
         FROM integrity_assessment a JOIN profile p ON p.id = a.profile_id \
         LEFT JOIN profile t ON t.id = a.seed_target \
         WHERE a.game = $1 AND a.score >= $2 \
           AND ($3::bigint IS NULL OR p.xuid = $3) \
           AND ($4::uuid IS NULL OR p.account_id = $4) \
           AND ($5::uuid IS NULL OR a.id < $5) \
         ORDER BY a.id DESC LIMIT $6",
    )
    .bind(game)
    .bind(min_score)
    .bind(xuid)
    .bind(account)
    .bind(before)
    .bind(limit.clamp(1, 200))
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(AssessmentRow {
            id: r.try_get("id")?,
            at: ms(r.try_get("at")?),
            game: r.try_get("game")?,
            xuid: r.try_get("xuid")?,
            mode: r.try_get("mode")?,
            source: r.try_get("source")?,
            session_id: r.try_get("play_session")?,
            hours: r.try_get("hours")?,
            deltas: r.try_get("deltas")?,
            rates: r.try_get("rates")?,
            score: r.try_get("score")?,
            reasons: r.try_get("reasons")?,
            refusal: r.try_get("refusal")?,
            decision: r.try_get("decision")?,
            would_block: r.try_get("would_block")?,
            rules_version: r.try_get("rules_version")?,
            revision: r.try_get("revision")?,
            audit: r.try_get("audit")?,
            bundle_sha256: r.try_get("bundle_sha256")?,
            seed_target: r.try_get("seed_target")?,
        });
    }
    Ok(out)
}

/// An accepted Hardcore revision the save audit has not read yet.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditDue {
    /// The game.
    pub game: String,
    /// The profile (its line).
    pub profile_id: Uuid,
    /// The revision.
    pub revision: i64,
    /// The play session that committed it.
    pub session_id: Uuid,
    /// The bundle's SHA-256 (its object in the save bucket).
    pub blob_sha256: Vec<u8>,
    /// The stored summary document (`fi_saves::SaveSummary`).
    pub summary: Value,
    /// The line's revision 1 when it is a `fresh_career` (a seeded line): its bundle's SHA-256,
    /// the seed it started from (`seeds::by_blob_in`). `None` for a Beta import or any other
    /// start, whose career began outside our seed.
    pub seed_blob: Option<Vec<u8>>,
}

/// Up to `limit` (1 to 1,000) accepted revisions of Hardcore profiles committed in the last
/// `max_age_days` (1 to 90) that have no `save_audit` assessment, oldest first. A `fresh_career`
/// revision is the seed's own bundle and is not audited; nor is a `fresh_start` (CR-S51), the
/// line's start the fresh-start check verified on receipt; nor a revision whose bundle retention
/// pruned (SAVES_API.md §8; retention keeps the queue's, so only a line past its 90 days).
pub async fn audit_due_in(
    conn: &mut PgConnection,
    max_age_days: i64,
    limit: i64,
) -> Result<Vec<AuditDue>, IdentityError> {
    let rows = sqlx::query(
        "SELECT p.game, r.profile_id, r.revision, r.session_id, r.blob_sha256, r.summary, \
           f.blob_sha256 AS seed_blob \
         FROM save_revision r JOIN profile p ON p.id = r.profile_id \
         LEFT JOIN save_revision f ON f.profile_id = r.profile_id AND f.revision = 1 \
           AND f.kind = 'fresh_career' \
         WHERE p.mode = 'hardcore' AND p.status = 'active' \
           AND r.kind NOT IN ('fresh_career', 'fresh_start') AND r.bundle_pruned_at IS NULL \
           AND r.committed_at > now() - make_interval(days => $1::int) \
           AND NOT EXISTS (SELECT 1 FROM integrity_assessment a \
             WHERE a.source = 'save_audit' AND a.profile_id = r.profile_id \
               AND a.revision = r.revision) \
         ORDER BY r.committed_at, r.profile_id, r.revision LIMIT $2",
    )
    .bind(max_age_days.clamp(1, 90) as i32)
    .bind(limit.clamp(1, 1_000))
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(AuditDue {
            game: r.try_get("game")?,
            profile_id: r.try_get("profile_id")?,
            revision: r.try_get("revision")?,
            session_id: r.try_get("session_id")?,
            blob_sha256: r.try_get("blob_sha256")?,
            summary: r.try_get("summary")?,
            seed_blob: r.try_get("seed_blob")?,
        });
    }
    Ok(out)
}

/// One save audit to record.
#[derive(Clone, Debug)]
pub struct AuditRecord<'a> {
    /// The revision audited (from [`audit_due_in`]).
    pub due: &'a AuditDue,
    /// 0-100.
    pub score: u8,
    /// Its reasons.
    pub reasons: &'a [fi_guard::ReasonCode],
    /// The audit's numbers (an object).
    pub audit: &'a Value,
}

/// Records a save audit in shadow (accepted, never a refusal). `false` when the revision already
/// has one (a rerun or a concurrent job): nothing is written.
pub async fn record_audit_in(
    conn: &mut PgConnection,
    record: &AuditRecord<'_>,
) -> Result<bool, IdentityError> {
    let reasons: Vec<&str> = record.reasons.iter().map(|r| r.code()).collect();
    let deltas = json!({
        "credits": null, "xp": null, "levels": null, "winnings": null, "cars": null,
        "unexplained": null, "game": null,
    });
    let rates = json!({
        "credits": 0.0, "unexplained": 0.0, "winnings": 0.0, "xp": 0.0, "levels": 0.0,
        "cars": 0.0, "dayUnexplained": 0.0, "dayWinnings": 0.0, "dayXp": 0.0, "dayLevels": 0.0,
    });
    let score = record.score.min(100);
    let inserted = sqlx::query(
        "INSERT INTO integrity_assessment \
           (game, profile_id, mode, play_session, source, hours, deltas, rates, score, reasons, \
            refusal, decision, would_block, rules_version, revision, audit) \
         VALUES ($1, $2, 'hardcore', $3, 'save_audit', 0, $4, $5, $6, $7, NULL, 'accepted', $8, \
                 $9, $10, $11) \
         ON CONFLICT (profile_id, revision) WHERE source = 'save_audit' DO NOTHING",
    )
    .bind(&record.due.game)
    .bind(record.due.profile_id)
    .bind(record.due.session_id)
    .bind(deltas)
    .bind(rates)
    .bind(i16::from(score))
    .bind(&reasons)
    .bind(score >= fi_guard::BLOCK)
    .bind(fi_guard::RULES_VERSION)
    .bind(record.due.revision)
    .bind(record.audit)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}

/// One nightly assessment of a committed revision to record ([`record_nightly_in`]).
#[derive(Clone, Copy, Debug)]
pub struct NightlyRecord<'a> {
    /// The game.
    pub game: &'a str,
    /// The profile (its line).
    pub profile_id: Uuid,
    /// The revision assessed.
    pub revision: i64,
    /// The play session that committed it.
    pub session_id: Uuid,
    /// The guardrails' assessment ([`fi_saves::nightly_assessment`]).
    pub assessment: &'a Assessment,
    /// What the inline guardrails would have refused it for (a ceiling's code, `ANOMALY_SCORE`
    /// with the enforcement switch on): recorded `flagged`; `None`: `accepted`.
    pub flag: Option<&'a str>,
}

/// Records the nightly pass's assessment of one revision (source `save_nightly`). `false` when the
/// revision already has one (a rerun or a concurrent job): nothing is written.
pub async fn record_nightly_in(
    conn: &mut PgConnection,
    record: &NightlyRecord<'_>,
) -> Result<bool, IdentityError> {
    let a = record.assessment;
    let reasons: Vec<&str> = a.reasons.iter().map(|r| r.code()).collect();
    let inserted = sqlx::query(
        "INSERT INTO integrity_assessment \
           (game, profile_id, mode, play_session, source, hours, deltas, rates, score, reasons, \
            refusal, decision, would_block, rules_version, revision) \
         VALUES ($1, $2, 'hardcore', $3, 'save_nightly', $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                 $13) \
         ON CONFLICT (profile_id, revision) WHERE source = 'save_nightly' DO NOTHING",
    )
    .bind(record.game)
    .bind(record.profile_id)
    .bind(record.session_id)
    .bind(a.hours.clamp(0.0, fi_guard::MAX_HOURS))
    .bind(deltas_json(a))
    .bind(rates_json(a))
    .bind(i16::from(a.score.min(100)))
    .bind(&reasons)
    .bind(record.flag)
    .bind(if record.flag.is_some() {
        "flagged"
    } else {
        "accepted"
    })
    .bind(a.would_block())
    .bind(fi_guard::RULES_VERSION)
    .bind(record.revision)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(inserted == 1)
}
