//! A Hardcore line seeded from another profile's save (CR-S52; SAVES_API.md §4.10, CLOUD_SAVES.md
//! §7.6, CORE_API.md §5; migration `20261027190000_seed_from_profile.sql`).
//!
//! - [`play_hours_in`]: the source profile's engaged play time recorded by this server, which the
//!   seed check measures the career over (the save itself carries no readable play time);
//! - [`CHECK_LIMIT`]: the dry runs per account (5 a day, counted before the check, refused or not);
//! - [`record_check_in`]: every check, dry run or commit, accepted or refused, recorded as an
//!   `integrity_assessment` of source `seed_check` by the bundle's SHA-256 (never its bytes), so the
//!   dry run cannot be used unseen to tune a save;
//! - [`revoke_in`]: the moderation action when the source is later found altered: the seeded line
//!   is retired (never leased again; retention prunes its bundles after 90 days) and the account
//!   is blocked pending review (`account.ban_review = 'seed_from_profile'`, answered as
//!   `ACCOUNTS.BANNED {review}`), in the caller's transaction.

use crate::identity::{IdentityError, ms};
use crate::limits::{self, Hit, Limit};
use fi_domain::UnixMs;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// Seed checks (dry runs) per account: 5 a day.
pub const CHECK_LIMIT: Limit = Limit {
    name: "saves.seed_check",
    max: 5,
    window_s: 86_400,
};

/// The `account.ban_review` of a seed revoke.
pub const REVIEW: &str = "seed_from_profile";

/// Longest stretch of one play session counted (an admitted session ends by 12 h, ACCOUNTS.md §7).
const SESSION_MAX_SECS: f64 = 12.0 * 3_600.0;

/// The rate-limit key of `account` (SHA-256 of the account id: no subject in clear).
pub fn limit_key(account: Uuid) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("seed:{account}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{hex}", CHECK_LIMIT.name)
}

/// Counts one seed check or `seed_from_profile` commit of `account` (in its own statement: a
/// refused one counts too).
pub async fn check_allowed(pool: &PgPool, account: Uuid) -> Result<Hit, IdentityError> {
    limits::hit(pool, &limit_key(account), &CHECK_LIMIT).await
}

/// How long after a session's last sign of life (heartbeat, game request, activity report) it is
/// still counted: one heartbeat interval (the lease TTL / 5 = 60 s) plus margin.
pub const ALIVE_GRACE_SECS: f64 = 120.0;

/// The engaged play time, in hours, this server recorded for `profile` (review H1): over its
/// admitted play sessions, each from admission to the earliest of its end and its last sign of
/// life (heartbeat or game request) plus [`ALIVE_GRACE_SECS`] (`ended_at` alone is when the
/// session was closed, possibly lazily hours later); where the hook reported activity (OD-121),
/// to its last progress or engagement plus the grace instead, less the idle episodes recorded for
/// it (`session_idle_observation.idle_secs`); each session at most 12 h. The save line's lease time
/// is not used (it counts idle time). Zero for a profile that never played here.
pub async fn play_hours_in(conn: &mut PgConnection, profile: Uuid) -> Result<f64, IdentityError> {
    let secs: f64 = sqlx::query_scalar(
        "WITH s AS ( \
           SELECT s.id, s.admitted_at, \
             LEAST(COALESCE(s.ended_at, clock_timestamp()), \
                   GREATEST(s.last_heartbeat_at, COALESCE(s.last_seen_at, s.last_heartbeat_at)) \
                     + make_interval(secs => $3), \
                   COALESCE(GREATEST(s.activity_progress_at, s.activity_engaged_at) \
                     + make_interval(secs => $3), 'infinity'::timestamptz)) AS until \
           FROM play_session s WHERE s.profile_id = $1 AND s.admitted_at IS NOT NULL), \
         e AS ( \
           SELECT GREATEST(0::float8, extract(epoch FROM (s.until - s.admitted_at))::float8 \
                    - COALESCE((SELECT sum(o.idle_secs) FROM session_idle_observation o \
                                WHERE o.session_id = s.id), 0)::float8) AS secs \
           FROM s) \
         SELECT COALESCE(sum(LEAST($2::float8, secs)), 0)::float8 FROM e",
    )
    .bind(profile)
    .bind(SESSION_MAX_SECS)
    .bind(ALIVE_GRACE_SECS)
    .fetch_one(&mut *conn)
    .await?;
    Ok(secs / 3_600.0)
}

/// One seed check to record.
#[derive(Clone, Copy, Debug)]
pub struct CheckRecord<'a> {
    /// The game.
    pub game: &'a str,
    /// The source profile (whose save was checked).
    pub source: Uuid,
    /// Its mode (`hardcore` or `noob`).
    pub source_mode: &'a str,
    /// The Hardcore profile it would seed.
    pub target: Uuid,
    /// The play session of a `seed_from_profile` commit; `None` for a dry run (nil UUID).
    pub session: Option<Uuid>,
    /// The bundle's SHA-256.
    pub blob_sha256: [u8; 32],
    /// The guardrails' assessment, when the check got that far.
    pub assessment: Option<&'a fi_guard::Assessment>,
    /// The hours measured over.
    pub hours: f64,
    /// Why it was refused (stable codes); empty when accepted.
    pub reasons: &'a [&'a str],
}

/// Records one seed check (`integrity_assessment`, source `seed_check`): refused with rule
/// `NOT_PLAUSIBLE_SEED` and the check's reasons, or accepted with the assessment's. Returns its id.
pub async fn record_check_in(
    conn: &mut PgConnection,
    r: &CheckRecord<'_>,
) -> Result<Uuid, IdentityError> {
    let refused = !r.reasons.is_empty();
    let reasons: Vec<&str> = if refused {
        r.reasons.to_vec()
    } else {
        r.assessment
            .map(|a| a.reasons.iter().map(|c| c.code()).collect())
            .unwrap_or_default()
    };
    let empty = serde_json::json!({});
    Ok(sqlx::query_scalar(
        "INSERT INTO integrity_assessment \
           (game, profile_id, mode, play_session, source, hours, deltas, rates, score, reasons, \
            refusal, decision, would_block, rules_version, bundle_sha256, seed_target) \
         VALUES ($1, $2, $3, $4, 'seed_check', $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         RETURNING id",
    )
    .bind(r.game)
    .bind(r.source)
    .bind(r.source_mode)
    .bind(r.session.unwrap_or(Uuid::nil()))
    .bind(r.hours.clamp(0.0, fi_guard::MAX_HOURS))
    .bind(
        r.assessment
            .map_or(empty.clone(), crate::integrity::deltas_json),
    )
    .bind(r.assessment.map_or(empty, crate::integrity::rates_json))
    .bind(i16::from(r.assessment.map_or(0, |a| a.score.min(100))))
    .bind(&reasons)
    .bind(refused.then_some(fi_saves::seed::NOT_PLAUSIBLE_SEED))
    .bind(if refused { "refused" } else { "accepted" })
    .bind(r.assessment.is_some_and(|a| a.would_block()))
    .bind(fi_guard::RULES_VERSION)
    .bind(r.blob_sha256.to_vec())
    .bind(r.target)
    .fetch_one(&mut *conn)
    .await?)
}

/// The seed checks recorded on `source` (newest first): `(seed_target, decision, reasons,
/// bundle_sha256)`. For tests and support.
pub async fn checks_of_in(
    conn: &mut PgConnection,
    source: Uuid,
) -> Result<Vec<(Uuid, String, Vec<String>, Vec<u8>)>, IdentityError> {
    let rows = sqlx::query(
        "SELECT seed_target, decision, reasons, bundle_sha256 FROM integrity_assessment \
         WHERE source = 'seed_check' AND profile_id = $1 ORDER BY at DESC, id DESC",
    )
    .bind(source)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get("seed_target")?,
                r.try_get("decision")?,
                r.try_get("reasons")?,
                r.try_get("bundle_sha256")?,
            ))
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Why [`revoke_in`] did nothing.
#[derive(Debug, thiserror::Error)]
pub enum RevokeError {
    /// No such live profile (`PORTAL.NOT_FOUND`).
    #[error("not found")]
    NotFound,
    /// The profile's line did not start from another profile (`PORTAL.CONFLICT {reason:
    /// "not_seeded"}`).
    #[error("not seeded from a profile")]
    NotSeeded,
    /// A database failure.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for RevokeError {
    fn from(e: sqlx::Error) -> Self {
        RevokeError::Db(e.into())
    }
}

/// What [`revoke_in`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Revoked {
    /// The seeded profile's account (blocked pending review).
    pub account_id: Uuid,
    /// The profile the line was seeded from.
    pub source: Uuid,
    /// When the line was retired (an earlier revoke's instant when it already was).
    pub retired_at: UnixMs,
    /// Refresh tokens, play sessions and MCP connections the block revoked.
    pub banned: crate::moderation::Banned,
}

/// The seed revoke of `profile` (a Hardcore profile whose revision 1 is `seed_from_profile`), in
/// the caller's transaction: the account is blocked pending review ([`REVIEW`], `reason` kept for
/// moderators; every refresh family, live play session and MCP connection revoked), then the line
/// is retired (lease cleared; admission refuses a retired line, so it is never leased again).
/// Lock order as the mode change's: the account, then the profile, then the head. Idempotent: a
/// line already retired keeps its instant.
pub async fn revoke_in(
    conn: &mut PgConnection,
    profile: Uuid,
    reason: &str,
) -> Result<Revoked, RevokeError> {
    let row = sqlx::query(
        "SELECT p.account_id, r.seed_source FROM profile p \
         LEFT JOIN save_revision r ON r.profile_id = p.id AND r.revision = 1 \
           AND r.kind = 'seed_from_profile' \
         WHERE p.id = $1 AND p.status <> 'deleted'",
    )
    .bind(profile)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(RevokeError::NotFound)?;
    let account_id: Uuid = row.try_get("account_id")?;
    let source: Uuid = row
        .try_get::<Option<Uuid>, _>("seed_source")?
        .ok_or(RevokeError::NotSeeded)?;
    let banned =
        crate::moderation::ban_with_review_in(conn, account_id, None, reason, Some(REVIEW))
            .await
            .map_err(|e| match e {
                IdentityError::NotFound => RevokeError::NotFound,
                e => RevokeError::Db(e),
            })?;
    sqlx::query("SELECT 1 FROM profile WHERE id = $1 FOR NO KEY UPDATE")
        .bind(profile)
        .execute(&mut *conn)
        .await?;
    let retired: time::OffsetDateTime = sqlx::query_scalar(
        "UPDATE save_head SET retired_at = COALESCE(retired_at, clock_timestamp()), \
           lease_session_id = NULL, lease_token = NULL, lease_device_id = NULL, \
           lease_device_label = NULL, lease_acquired_at = NULL, lease_expires_at = NULL, \
           lease_ttl_ms = NULL, lease_accounted_at = NULL \
         WHERE profile_id = $1 RETURNING retired_at",
    )
    .bind(profile)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Revoked {
        account_id,
        source,
        retired_at: ms(retired),
        banned,
    })
}

/// Whether `source` already seeded a live (not retired) line other than `target`'s (review L7: one
/// source seeds at most one Hardcore line at a time).
pub async fn source_in_use_in(
    conn: &mut PgConnection,
    source: Uuid,
    target: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM save_revision r JOIN save_head h ON h.profile_id = r.profile_id \
           WHERE r.seed_source = $1 AND r.profile_id <> $2 AND h.retired_at IS NULL)",
    )
    .bind(source)
    .bind(target)
    .fetch_one(&mut *conn)
    .await?)
}

/// Whether `commit_id` is already a revision of `target`'s line: the replay of a committed
/// `seed_from_profile` upload, answered by the save core without counting a check (a launcher whose
/// 201 was lost must not be rate-limited out of its own commit).
pub async fn commit_landed_in(
    conn: &mut PgConnection,
    target: Uuid,
    commit_id: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM save_revision WHERE profile_id = $1 AND commit_id = $2)",
    )
    .bind(target)
    .bind(commit_id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Whether `commit_id` already committed exactly these bytes on `target`: a true replay (the same
/// key with other bytes is no replay and is counted as a new commit).
pub async fn commit_landed_with_in(
    conn: &mut PgConnection,
    target: Uuid,
    commit_id: Uuid,
    blob_sha256: &[u8; 32],
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM save_revision \
         WHERE profile_id = $1 AND commit_id = $2 AND blob_sha256 = $3)",
    )
    .bind(target)
    .bind(commit_id)
    .bind(blob_sha256.as_slice())
    .fetch_one(&mut *conn)
    .await?)
}

/// Whether a refused `seed_from_profile` commit of these bytes on `target` is recorded already
/// (review L5: the same bundle sent again on the same, empty head is not recorded twice).
pub async fn refused_commit_recorded_in(
    conn: &mut PgConnection,
    target: Uuid,
    blob_sha256: &[u8; 32],
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM integrity_assessment WHERE source = 'seed_check' \
           AND seed_target = $1 AND bundle_sha256 = $2 AND decision = 'refused' \
           AND play_session <> '00000000-0000-0000-0000-000000000000'::uuid)",
    )
    .bind(target)
    .bind(blob_sha256.to_vec())
    .fetch_one(&mut *conn)
    .await?)
}
