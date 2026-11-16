//! The Postgres save store (CLOUD_SAVES.md §5, §10).
//!
//! Every write starts by locking the profile's `save_head` row (`SELECT ... FOR UPDATE`), reads
//! the database clock **after** the lock is held (`clock_timestamp()`, so time spent waiting for
//! the lock is not counted against an expiry), runs the shared decision of `fi_saves::lease`, and
//! writes. One row lock per profile: two writes to the same line serialize, writes to different
//! lines never contend, and no transaction locks two heads, so there is no deadlock. A commit
//! inserts the revision, moves the head, writes the audit row and the profile's `profile_progress`
//! numbers in the same transaction (no outbox, owner 2026-10-26). The deferred foreign key
//! `save_head_points_at_its_revision` and the `fi_save_head_guard` trigger make it impossible to
//! commit a head that names a missing revision or moves backwards.
//!
//! Each write exists twice: as a function over the caller's connection ([`acquire_in`],
//! [`renew_in`], [`release_in`], [`commit_in`]), so the account plane can lock a pool, grant the
//! lease and open the play session in **one** transaction (ADR 0008 §2: lock
//! order pool, then head; on `Err` the caller rolls back), and as the [`SaveStore`] method, which
//! runs it in a transaction of its own.

use fi_domain::{DeviceId, PlaySessionId, ProfileId, ProfileMode, UnixMs};
use fi_saves::lease::{self, LineState};
use fi_saves::{
    AcquireMode, AcquireRequest, Actor, BlobRef, CommitId, CommitKind, CommitRequest, Committed,
    Digest, FenceToken, GuardInput, HeadView, Lease, LeaseGrant, LeaseHolder, ParkReason,
    ParkRequest, ParkedRecord, Released, Replaced, Revision, RevisionRecord, SaveError, SaveStore,
    SaveSummary,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

/// `fi_saves::SaveStore` over Postgres.
#[derive(Clone)]
pub struct PgSaveStore {
    pool: PgPool,
    eligible: bool,
}

impl PgSaveStore {
    /// A store over `pool` (a `fi_runtime` login).
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            eligible: false,
        }
    }

    /// Newest-first history of the revisions that still have their save file (retention prunes
    /// the bundles of older ones, SAVES_API.md §8): what the launcher's history route lists, since
    /// only these can be downloaded or restored. [`SaveStore::history`] lists every row (the gates
    /// read them all).
    pub async fn history_with_bundles(
        &self,
        profile: ProfileId,
        before: Option<Revision>,
        limit: u32,
    ) -> Result<Vec<RevisionRecord>, SaveError> {
        self.history_rows(profile, before, limit, true).await
    }

    async fn history_rows(
        &self,
        profile: ProfileId,
        before: Option<Revision>,
        limit: u32,
        with_bundles: bool,
    ) -> Result<Vec<RevisionRecord>, SaveError> {
        let kept = if with_bundles {
            "AND r.bundle_pruned_at IS NULL"
        } else {
            ""
        };
        let sql = format!(
            "SELECT {REVISION_COLUMNS} FROM save_head h \
             LEFT JOIN save_revision r ON r.profile_id = h.profile_id AND r.revision < $2 {kept} \
             LEFT JOIN save_blob b ON b.sha256 = r.blob_sha256 \
             WHERE h.profile_id = $1 ORDER BY r.revision DESC NULLS LAST LIMIT $3"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(profile.as_uuid())
            .bind(before.map_or(i64::MAX, |b| b.get() as i64))
            .bind(i64::from(limit.max(1)))
            .fetch_all(&self.pool)
            .await
            .map_err(unavailable)?;
        if rows.is_empty() {
            return Err(SaveError::ProfileNotFound);
        }
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            if row
                .try_get::<Option<i64>, _>("revision")
                .map_err(unavailable)?
                .is_none()
            {
                break;
            }
            out.push(revision_from_row(row)?);
        }
        out.truncate(limit as usize);
        Ok(out)
    }

    /// The same store refusing a player's upload for a profile that keeps no cloud data now under
    /// the default rule (OD-52; [`commit_checked_in`]). The save core's conformance runs without
    /// it (its profiles have no Supporter account).
    pub fn eligible_only(self) -> Self {
        Self {
            eligible: true,
            ..self
        }
    }

    /// The pool the store runs on.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn unavailable(e: sqlx::Error) -> SaveError {
    match &e {
        // 55P03 lock_not_available (lock_timeout), 57014 query_canceled (statement_timeout),
        // 25P04 transaction_timeout.
        sqlx::Error::Database(db)
            if matches!(db.code().as_deref(), Some("55P03" | "57014" | "25P04")) =>
        {
            SaveError::Unavailable("save storage busy, retry".into())
        }
        _ => SaveError::Unavailable(e.to_string()),
    }
}

fn is_foreign_key_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23503"))
}

fn to_ts(t: UnixMs) -> Result<OffsetDateTime, SaveError> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(t.millis()) * 1_000_000)
        .map_err(|_| SaveError::InvalidArgument("instant out of range"))
}

fn from_ts(t: OffsetDateTime) -> UnixMs {
    UnixMs::from_millis((t.unix_timestamp_nanos().div_euclid(1_000_000)) as i64)
}

fn to_i64(v: u64) -> Result<i64, SaveError> {
    i64::try_from(v).map_err(|_| SaveError::InvalidArgument("number out of range"))
}

fn digest(row: &PgRow, column: &str) -> Result<Digest, SaveError> {
    let bytes: Vec<u8> = row.try_get(column).map_err(unavailable)?;
    Digest::from_slice(&bytes)
        .ok_or_else(|| SaveError::Unavailable(format!("{column} is not 32 bytes")))
}

fn summary_json(s: &SaveSummary) -> serde_json::Value {
    let mut v =
        serde_json::json!({ "credits": s.credits, "xp": s.xp, "level": s.level, "cars": s.cars });
    // The guardrails' numbers (OD-62), only when the bundle had them readable.
    for (key, value) in [
        ("winnings", s.winnings),
        ("tokens", s.tokens),
        ("skillPoints", s.skill_points),
        ("unspentSkillPoints", s.unspent_skill_points),
    ] {
        if let Some(n) = value {
            v[key] = n.into();
        }
    }
    v
}

fn mode_of(text: &str) -> Result<ProfileMode, SaveError> {
    text.parse()
        .map_err(|_| SaveError::Unavailable("profile has an invalid mode".into()))
}

/// What the commit and lease paths read under the row lock.
struct Locked {
    mode: ProfileMode,
    revision: Revision,
    fence_seq: u64,
    reset_revision: Revision,
    lease: Option<Lease>,
    account_id: Uuid,
    game: String,
    now: UnixMs,
}

impl Locked {
    fn state(&self) -> LineState<'_> {
        LineState {
            mode: self.mode,
            revision: self.revision,
            fence_seq: self.fence_seq,
            reset_revision: self.reset_revision,
            lease: self.lease.as_ref(),
        }
    }

    /// Play time to credit before this write ([`lease::play_credit`]).
    fn credit(&self) -> Result<i64, SaveError> {
        to_i64(lease::play_credit(self.lease.as_ref(), self.now))
    }
}

/// The newest revision of `profile`'s restore chain from `from` (itself, its `restored_from`,
/// recursively) that the nightly guardrails pass flagged (`integrity_assessment` source
/// `save_nightly`, decision `flagged`), if any. A flag older than the assessments' retention (90
/// days) no longer holds a restore.
pub async fn flagged_in_chain(
    conn: &mut PgConnection,
    profile: ProfileId,
    from: Revision,
) -> Result<Option<Revision>, SaveError> {
    let flagged: Option<i64> = sqlx::query_scalar(
        "WITH RECURSIVE chain (revision) AS ( \
           SELECT $2::bigint \
           UNION \
           SELECT r.restored_from FROM chain c JOIN save_revision r \
             ON r.profile_id = $1 AND r.revision = c.revision WHERE r.restored_from IS NOT NULL) \
         SELECT max(a.revision) FROM integrity_assessment a JOIN chain c ON c.revision = a.revision \
          WHERE a.profile_id = $1 AND a.source = 'save_nightly' AND a.decision = 'flagged'",
    )
    .bind(profile.as_uuid())
    .bind(to_i64(from.get())?)
    .fetch_one(&mut *conn)
    .await
    .map_err(unavailable)?;
    Ok(flagged.map(|r| Revision::new(r as u64)))
}

/// `save_revision.guard_input` (hybrid Hardcore checks, CLOUD_SAVES.md §7.5): the game sync a
/// gated upload was compared with and the play time it was measured at, for the nightly pass
/// (`{"game": null, "playMs": n}` before any sync; `hoursSinceBits` holds the hours' exact `f64`
/// bits, `hoursSince` the readable value).
pub fn guard_input_json(input: &GuardInput) -> serde_json::Value {
    serde_json::json!({
        "game": input.game.map(|g| serde_json::json!({
            "credits": g.credits,
            "xp": g.xp,
            "level": g.level,
            "hoursSince": if g.hours_since.is_finite() { g.hours_since } else { 0.0 },
            // The exact value the upload was measured with (a JSON decimal may read back one ulp
            // off): the nightly pass reproduces the assessment bit for bit.
            "hoursSinceBits": if g.hours_since.is_finite() { g.hours_since.to_bits() } else { 0 },
        })),
        "playMs": input.play_ms,
    })
}

/// [`guard_input_json`] read back; `None` when it is not that shape.
pub fn guard_input_from_json(value: &serde_json::Value) -> Option<GuardInput> {
    let game = match value.get("game")? {
        serde_json::Value::Null => None,
        g => Some(fi_guard::GameSync {
            credits: g.get("credits")?.as_i64()?,
            xp: g.get("xp")?.as_i64()?,
            level: g.get("level")?.as_i64()?,
            hours_since: match g.get("hoursSinceBits").and_then(serde_json::Value::as_u64) {
                Some(bits) => f64::from_bits(bits),
                None => g.get("hoursSince")?.as_f64()?,
            },
        }),
    };
    let play_ms = match value.get("playMs") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(v.as_u64()?),
    };
    Some(GuardInput { game, play_ms })
}

// `line_*` names, not the bare ones: the head query also selects the joined revision's columns.
const HEAD_COLUMNS: &str = "h.revision AS line_revision, h.fence_seq, h.reset_revision, h.play_ms AS line_play_ms, \
    h.lease_session_id, h.lease_token, h.lease_device_id, h.lease_device_label, h.lease_acquired_at, h.lease_expires_at, \
    h.lease_ttl_ms, h.lease_accounted_at";

fn lease_from_row(row: &PgRow) -> Result<Option<Lease>, SaveError> {
    let token: Option<i64> = row.try_get("lease_token").map_err(unavailable)?;
    let Some(token) = token else { return Ok(None) };
    let session: Uuid = row.try_get("lease_session_id").map_err(unavailable)?;
    let device: Uuid = row.try_get("lease_device_id").map_err(unavailable)?;
    let corrupt =
        |what: &str| SaveError::Unavailable(format!("save_head lease has an invalid {what}"));
    Ok(Some(Lease {
        holder: LeaseHolder {
            session: PlaySessionId::from_uuid(session).ok_or_else(|| corrupt("session"))?,
            device: DeviceId::from_uuid(device).ok_or_else(|| corrupt("device"))?,
            device_label: row.try_get("lease_device_label").map_err(unavailable)?,
        },
        token: FenceToken::new(token as u64).ok_or_else(|| corrupt("token"))?,
        acquired_at: from_ts(row.try_get("lease_acquired_at").map_err(unavailable)?),
        expires_at: from_ts(row.try_get("lease_expires_at").map_err(unavailable)?),
        ttl_ms: row.try_get::<i32, _>("lease_ttl_ms").map_err(unavailable)? as u32,
        accounted_at: from_ts(row.try_get("lease_accounted_at").map_err(unavailable)?),
    }))
}

fn revision_from_row(row: &PgRow) -> Result<RevisionRecord, SaveError> {
    let corrupt =
        |what: &str| SaveError::Unavailable(format!("save_revision has an invalid {what}"));
    let kind: String = row.try_get("kind").map_err(unavailable)?;
    let actor: String = row.try_get("actor").map_err(unavailable)?;
    let summary: serde_json::Value = row.try_get("summary").map_err(unavailable)?;
    let device: Option<Uuid> = row.try_get("device_id").map_err(unavailable)?;
    let restored_from: Option<i64> = row.try_get("restored_from").map_err(unavailable)?;
    Ok(RevisionRecord {
        profile: ProfileId::from_uuid(row.try_get("profile_id").map_err(unavailable)?)
            .ok_or_else(|| corrupt("profile"))?,
        revision: Revision::new(row.try_get::<i64, _>("revision").map_err(unavailable)? as u64),
        commit_id: CommitId::from_uuid(row.try_get("commit_id").map_err(unavailable)?)
            .ok_or_else(|| corrupt("commit id"))?,
        blob: BlobRef {
            sha256: digest(row, "blob_sha256")?,
            size: row.try_get::<i32, _>("size_bytes").map_err(unavailable)? as u32,
            format: None,
        },
        content: digest(row, "content_sha256")?,
        kind: CommitKind::parse(&kind).ok_or_else(|| corrupt("kind"))?,
        actor: Actor::parse(&actor).ok_or_else(|| corrupt("actor"))?,
        session: PlaySessionId::from_uuid(row.try_get("session_id").map_err(unavailable)?)
            .ok_or_else(|| corrupt("session"))?,
        token: FenceToken::new(row.try_get::<i64, _>("fence_token").map_err(unavailable)? as u64)
            .ok_or_else(|| corrupt("token"))?,
        device: device.and_then(DeviceId::from_uuid),
        client_label: row.try_get("client_label").map_err(unavailable)?,
        restored_from: restored_from.map(|r| Revision::new(r as u64)),
        parked_from: row.try_get("parked_from").map_err(unavailable)?,
        summary: serde_json::from_value(summary).map_err(|_| corrupt("summary"))?,
        committed_at: from_ts(row.try_get("committed_at").map_err(unavailable)?),
        play_ms: row.try_get::<i64, _>("play_ms").map_err(unavailable)? as u64,
        pinned: row.try_get("pinned").map_err(unavailable)?,
    })
}

/// The kinds the guardrails' rates start from (`CommitKind::is_rate_anchor`; the
/// `rate_anchors_match_the_model` test keeps them equal).
const RATE_ANCHORS: &str = "'fresh_career', 'beta_import', 'moderation_reset', 'season_reset', \
    'career_reset', 'parked_accept', 'fresh_start', 'seed_from_profile'";

/// A revision whose credits, XP and level are readable (`SaveSummary::is_complete_for_gates`).
const READABLE: &str = "jsonb_typeof(r.summary -> 'credits') = 'number' \
    AND jsonb_typeof(r.summary -> 'xp') = 'number' AND jsonb_typeof(r.summary -> 'level') = 'number'";

const REVISION_COLUMNS: &str = "r.profile_id, r.revision, r.commit_id, r.blob_sha256, b.size_bytes, r.content_sha256, r.kind, r.actor, \
    r.session_id, r.fence_token, r.device_id, r.client_label, r.restored_from, r.parked_from, r.summary, r.committed_at, \
    r.play_ms, r.pinned";

/// Locks the line and reads it, with the database clock read after the lock is held.
async fn lock_line(conn: &mut PgConnection, profile: ProfileId) -> Result<Locked, SaveError> {
    let sql = format!(
        "SELECT {HEAD_COLUMNS}, p.account_id, p.game, p.mode \
         FROM save_head h JOIN profile p ON p.id = h.profile_id \
         WHERE h.profile_id = $1 FOR UPDATE OF h"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(profile.as_uuid())
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?
        .ok_or(SaveError::ProfileNotFound)?;
    let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await
        .map_err(unavailable)?;
    let mode: String = row.try_get("mode").map_err(unavailable)?;
    Ok(Locked {
        mode: mode_of(&mode)?,
        revision: Revision::new(
            row.try_get::<i64, _>("line_revision")
                .map_err(unavailable)? as u64,
        ),
        fence_seq: row.try_get::<i64, _>("fence_seq").map_err(unavailable)? as u64,
        reset_revision: Revision::new(
            row.try_get::<i64, _>("reset_revision")
                .map_err(unavailable)? as u64,
        ),
        lease: lease_from_row(&row)?,
        account_id: row.try_get("account_id").map_err(unavailable)?,
        game: row.try_get("game").map_err(unavailable)?,
        now: from_ts(now),
    })
}

async fn audit(
    conn: &mut PgConnection,
    profile: ProfileId,
    kind: &'static str,
    session: Option<PlaySessionId>,
    token: Option<FenceToken>,
    revision: Option<Revision>,
    detail: serde_json::Value,
) -> Result<(), SaveError> {
    sqlx::query("INSERT INTO save_event (profile_id, kind, session_id, fence_token, revision, detail) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(profile.as_uuid())
        .bind(kind)
        .bind(session.map(|s| s.as_uuid()))
        .bind(token.map(|t| t.get() as i64))
        .bind(revision.map(|r| r.get() as i64))
        .bind(detail)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
    Ok(())
}

fn replaced_detail(replaced: &Option<Replaced>) -> serde_json::Value {
    let describe = |why: &str, l: &Lease| serde_json::json!({ "replaced": why, "token": l.token.get(), "session": l.holder.session.to_string(), "device": l.holder.device.to_string() });
    match replaced {
        None => serde_json::json!({}),
        Some(Replaced::Expired(l)) => describe("expired", l),
        Some(Replaced::SameDevice(l)) => describe("same_device", l),
        Some(Replaced::TakenOver(l)) => describe("taken_over", l),
    }
}

async fn write_lease(
    conn: &mut PgConnection,
    profile: ProfileId,
    lease: &Lease,
    credit_ms: i64,
) -> Result<(), SaveError> {
    sqlx::query(
        "UPDATE save_head SET fence_seq = GREATEST(fence_seq, $2), lease_session_id = $3, lease_token = $2, lease_device_id = $4, \
         lease_device_label = $5, lease_acquired_at = $6, lease_expires_at = $7, lease_ttl_ms = $8, lease_accounted_at = $9, \
         play_ms = play_ms + $10 WHERE profile_id = $1",
    )
    .bind(profile.as_uuid())
    .bind(lease.token.get() as i64)
    .bind(lease.holder.session.as_uuid())
    .bind(lease.holder.device.as_uuid())
    .bind(&lease.holder.device_label)
    .bind(to_ts(lease.acquired_at)?)
    .bind(to_ts(lease.expires_at)?)
    .bind(lease.ttl_ms as i32)
    .bind(to_ts(lease.accounted_at)?)
    .bind(credit_ms)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    Ok(())
}

/// Registers a blob row, or clears its garbage-collection mark (and `object_deleted_at`) if it was
/// marked (CLOUD_SAVES.md §6). Either way the row is locked until the transaction ends, so the
/// collector cannot delete it in between. `true` when the row was created **or was marked**: the
/// caller then re-puts the object, which a collection may have deleted (the row deleted with it, or
/// kept with `object_deleted_at` because only pruned revisions name it, or kept marked because the
/// object delete succeeded and the row's did not).
///
/// The row's `format` (`fi_saves::SaveFormat::name`, CR-S105) is the one the bytes' magic names
/// ([`BlobRef::format`], review 4); `line` (the line's game's) only for bytes of neither magic (an
/// unreadable bundle parked) or a ref built without its bytes. A row that exists keeps its own
/// (the digest names the bytes, whose magic names the format).
pub(crate) async fn insert_blob(
    conn: &mut PgConnection,
    blob: &BlobRef,
    line: fi_saves::SaveFormat,
) -> Result<bool, SaveError> {
    let format = blob.format.unwrap_or(line);
    // RETURNING yields a row when the insert happened or a mark was cleared; nothing when the row
    // existed unmarked (still locked), whose object no collection touched.
    let created: Option<bool> = sqlx::query_scalar(
        "INSERT INTO save_blob (sha256, size_bytes, storage_key, format) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (sha256) DO UPDATE SET gc_marked_at = NULL, object_deleted_at = NULL \
           WHERE save_blob.gc_marked_at IS NOT NULL OR save_blob.object_deleted_at IS NOT NULL \
         RETURNING true",
    )
    .bind(blob.sha256.0.to_vec())
    .bind(blob.size as i32)
    .bind(blob.storage_key())
    .bind(format.name())
    .fetch_optional(&mut *conn)
    .await
    .map_err(unavailable)?;
    Ok(created.unwrap_or(false))
}

async fn begin(pool: &PgPool) -> Result<Transaction<'static, Postgres>, SaveError> {
    pool.begin().await.map_err(unavailable)
}

/// [`SaveStore::acquire`] in the caller's transaction.
pub async fn acquire_in(
    conn: &mut PgConnection,
    profile: ProfileId,
    req: &AcquireRequest,
) -> Result<LeaseGrant, SaveError> {
    let line = lock_line(conn, profile).await?;
    let grant = lease::decide_acquire(line.state(), req, line.now)?;
    if !grant.existing {
        write_lease(conn, profile, &grant.lease, line.credit()?).await?;
        let kind = match (req.mode, &grant.replaced) {
            (_, Some(Replaced::TakenOver(_))) => "lease_taken_over",
            (AcquireMode::Resume { .. }, _) => "lease_resumed",
            _ => "lease_granted",
        };
        let detail = replaced_detail(&grant.replaced);
        audit(
            conn,
            profile,
            kind,
            Some(req.holder.session),
            Some(grant.lease.token),
            None,
            detail,
        )
        .await?;
    }
    Ok(grant)
}

/// [`SaveStore::renew`] in the caller's transaction.
pub async fn renew_in(
    conn: &mut PgConnection,
    profile: ProfileId,
    session: PlaySessionId,
    token: FenceToken,
    ttl_ms: u32,
) -> Result<Lease, SaveError> {
    let line = lock_line(conn, profile).await?;
    let renewed = lease::decide_renew(line.state(), session, token, ttl_ms, line.now)?;
    sqlx::query(
        "UPDATE save_head SET lease_expires_at = $2, lease_ttl_ms = $3, lease_accounted_at = $4, play_ms = play_ms + $5 \
         WHERE profile_id = $1",
    )
    .bind(profile.as_uuid())
    .bind(to_ts(renewed.expires_at)?)
    .bind(renewed.ttl_ms as i32)
    .bind(to_ts(renewed.accounted_at)?)
    .bind(line.credit()?)
    .execute(&mut *conn)
    .await
    .map_err(unavailable)?;
    Ok(renewed)
}

/// [`SaveStore::release`] in the caller's transaction.
pub async fn release_in(
    conn: &mut PgConnection,
    profile: ProfileId,
    session: PlaySessionId,
    token: FenceToken,
) -> Result<Released, SaveError> {
    let line = lock_line(conn, profile).await?;
    let outcome = lease::decide_release(line.state(), session, token)?;
    if outcome == Released::Released {
        sqlx::query(
            "UPDATE save_head SET lease_session_id = NULL, lease_token = NULL, lease_device_id = NULL, lease_device_label = NULL, \
             lease_acquired_at = NULL, lease_expires_at = NULL, lease_ttl_ms = NULL, lease_accounted_at = NULL, \
             play_ms = play_ms + $2 WHERE profile_id = $1",
        )
        .bind(profile.as_uuid())
        .bind(line.credit()?)
        .execute(&mut *conn)
        .await
        .map_err(unavailable)?;
        audit(
            conn,
            profile,
            "lease_released",
            Some(session),
            Some(token),
            None,
            serde_json::json!({}),
        )
        .await?;
    }
    Ok(outcome)
}

/// [`SaveStore::commit`] in the caller's transaction. A replay returns the first revision and
/// writes nothing; on `Err` the caller rolls back (and may audit the refusal, as the trait method
/// does).
pub async fn commit_in(
    conn: &mut PgConnection,
    req: &CommitRequest,
) -> Result<Committed, SaveError> {
    commit_checked_in(conn, req, false).await
}

/// CR-S51 (SAVES_API.md §4.9, GAME_PLATFORM.md §3a): a verified `fresh_start` revision 1 sets each
/// missing half of the profile's Hardcore RPC baseline from its numbers, no recent growth (a half
/// the game's syncs already set is kept). Lock order as the seed's: the line, then the profile's
/// integrity lock.
async fn fresh_start_baseline_in(
    conn: &mut PgConnection,
    game: &str,
    req: &CommitRequest,
) -> Result<(), SaveError> {
    let db = |e: crate::identity::IdentityError| SaveError::Unavailable(e.to_string());
    crate::integrity::lock_in(conn, game, req.profile.as_uuid())
        .await
        .map_err(db)?;
    let subject = crate::integrity::Subject {
        game,
        profile_id: req.profile.as_uuid(),
        mode: ProfileMode::Hardcore.as_str(),
    };
    let s = &req.summary;
    let numbers = crate::integrity::ProfileNumbers {
        credits: s.credits.unwrap_or(0),
        xp: s.xp.unwrap_or(0),
        level: s.level.unwrap_or(0),
        winnings: s.winnings.unwrap_or(0),
        top_speed_mps: 0.0,
    };
    crate::seeds::profile_baseline_of_in(conn, subject, &numbers)
        .await
        .map_err(db)?;
    crate::seeds::garage_baseline_of_in(conn, subject, s.cars.unwrap_or(0))
        .await
        .map_err(db)?;
    Ok(())
}

/// [`commit_in`], and with `eligible` (OD-239: Hardcore only, CR-S111) a player's upload or
/// the line's fresh career is refused with [`SaveError::NotEligible`] unless the profile keeps a
/// line now ([`crate::cloud::refusal_in`]): checked under the line's
/// lock, after a replay is answered, so a lapse or a mode change that lands before the commit
/// refuses it instead of letting it in. A `fresh_start` (CR-S51) also sets each missing half of
/// the profile's RPC baseline from its numbers, in the same transaction (GAME_PLATFORM.md §3a).
pub async fn commit_checked_in(
    conn: &mut PgConnection,
    req: &CommitRequest,
    eligible: bool,
) -> Result<Committed, SaveError> {
    let line = lock_line(conn, req.profile).await?;
    // Under the lock, so a racing copy of this commit is either fully visible or absent.
    if let Some(record) = find_commit_on(conn, req.commit_id).await? {
        return if record.profile == req.profile && record.blob.sha256 == req.blob.sha256 {
            Ok(Committed {
                record,
                replayed: true,
                blob_created: false,
            })
        } else {
            Err(SaveError::IdempotencyKeyReused)
        };
    }
    if eligible
        && (req.kind.is_player_upload() || req.kind == CommitKind::FreshCareer)
        && let Some((tier, mode)) = crate::cloud::refusal_in(conn, line.account_id, req.profile)
            .await
            .map_err(|e| SaveError::Unavailable(e.to_string()))?
    {
        return Err(SaveError::NotEligible {
            tier: tier.as_str().to_owned(),
            mode,
        });
    }
    let extended = lease::check_commit(line.state(), req, line.now)?;
    // CR-S52 review L7: one source seeds at most one live line, checked under a lock on the source
    // (two targets' commits lock different lines).
    if let Some(source) = req.seed_source {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('fi.seed_source'), hashtext($1))")
            .bind(source.as_uuid().to_string())
            .execute(&mut *conn)
            .await
            .map_err(unavailable)?;
        if crate::seed_profile::source_in_use_in(conn, source.as_uuid(), req.profile.as_uuid())
            .await
            .map_err(|e| SaveError::Unavailable(e.to_string()))?
        {
            return Err(SaveError::HardcoreNeedsFreshCareer);
        }
    }
    // A restore copies its source's bundle: refused once retention pruned it (under the lock
    // retention also holds, so a source cannot be pruned between this check and the commit).
    if let Some(from) = req.restored_from {
        let kept: Option<bool> = sqlx::query_scalar(
            "SELECT bundle_pruned_at IS NULL FROM save_revision WHERE profile_id = $1 AND revision = $2",
        )
        .bind(req.profile.as_uuid())
        .bind(to_i64(from.get())?)
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?;
        if kept != Some(true) {
            return Err(SaveError::RevisionNotFound(from));
        }
        // A player never restores a revision the nightly guardrails pass flagged, nor a restore
        // whose chain reaches one (hybrid checks, CLOUD_SAVES.md §7.5): a moderator decides.
        if req.actor == Actor::Player
            && let Some(flagged) = flagged_in_chain(conn, req.profile, from).await?
        {
            return Err(SaveError::RestoreFlagged { revision: flagged });
        }
    }
    let credit = line.credit()?;
    let revision = line.revision.next();
    let reset_revision = lease::reset_after(req.kind, revision, line.reset_revision);
    let blob_created =
        insert_blob(conn, &req.blob, fi_saves::SaveFormat::of_game(&line.game)).await?;
    // A player's commit is the launcher syncing again: it clears the renewal mark (OD-52,
    // `crate::cloud`).
    let play_ms: i64 = sqlx::query_scalar(
        "UPDATE save_head SET revision = $2, lease_expires_at = $3, lease_accounted_at = $4, play_ms = play_ms + $5, \
         reset_revision = $6, lapsed_at = CASE WHEN $7 THEN NULL ELSE lapsed_at END WHERE profile_id = $1 RETURNING play_ms",
    )
    .bind(req.profile.as_uuid())
    .bind(to_i64(revision.get())?)
    .bind(to_ts(extended.expires_at)?)
    .bind(to_ts(extended.accounted_at)?)
    .bind(credit)
    .bind(to_i64(reset_revision.get())?)
    .bind(req.kind.is_player_upload())
    .fetch_one(&mut *conn)
    .await
    .map_err(unavailable)?;
    sqlx::query(
        "INSERT INTO save_revision (profile_id, revision, commit_id, blob_sha256, content_sha256, kind, actor, session_id, fence_token, \
         device_id, client_label, restored_from, parked_from, summary, committed_at, play_ms, guard_input, seed_source) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
    )
    .bind(req.profile.as_uuid())
    .bind(to_i64(revision.get())?)
    .bind(req.commit_id.as_uuid())
    .bind(req.blob.sha256.0.to_vec())
    .bind(req.content.0.to_vec())
    .bind(req.kind.as_str())
    .bind(req.actor.as_str())
    .bind(req.session.as_uuid())
    .bind(to_i64(req.token.get())?)
    .bind(req.device.map(|d| d.as_uuid()))
    .bind(req.client_label.as_deref())
    .bind(req.restored_from.map(|r| r.get() as i64))
    .bind(req.parked_from)
    .bind(summary_json(&req.summary))
    .bind(to_ts(line.now)?)
    .bind(play_ms)
    .bind(req.guard_input.as_ref().map(guard_input_json))
    .bind(req.seed_source.map(|p| p.as_uuid()))
    .execute(&mut *conn)
    .await
    .map_err(|e| {
        if is_foreign_key_violation(&e) {
            SaveError::InvalidArgument("the revision names a parked bundle that does not exist")
        } else {
            unavailable(e)
        }
    })?;
    let detail = serde_json::json!({
        "kind": req.kind.as_str(), "content": req.content.hex(), "blob": req.blob.sha256.hex(),
        "gatedBase": req.gated_base.map(Revision::get), "playMs": play_ms,
        "seedSource": req.seed_source.map(|p| p.as_uuid().to_string()),
    });
    audit(
        conn,
        req.profile,
        "commit",
        Some(req.session),
        Some(req.token),
        Some(revision),
        detail,
    )
    .await?;
    // A verified first revision of the player's bytes (CR-S51's fresh start, CR-S52's seed from a
    // profile) is the RPC baseline's start, as the server's seed is.
    if matches!(
        req.kind,
        CommitKind::FreshStart | CommitKind::SeedFromProfile
    ) {
        fresh_start_baseline_in(conn, &line.game, req).await?;
    }
    // The launcher's numbers (`profile_progress`), synchronously in the commit's transaction. An
    // FM5 career's are shared with its online sync (CR-S104): the newest write wins, and a number
    // the save does not carry keeps the sync's (CR-S105, `crate::fm5::save_progress_in`).
    if line.game == crate::fm5::GAME {
        crate::progression::fm5_save_committed_guarded(
            conn,
            req.profile.as_uuid(),
            to_i64(revision.get())?,
            &req.summary,
        )
        .await
        .map_err(unavailable)?;
    } else {
        crate::progression::save_committed_guarded(
            conn,
            req.profile.as_uuid(),
            to_i64(revision.get())?,
            &req.summary,
        )
        .await
        .map_err(unavailable)?;
    }
    Ok(Committed {
        record: RevisionRecord {
            profile: req.profile,
            revision,
            commit_id: req.commit_id,
            blob: req.blob,
            content: req.content,
            kind: req.kind,
            actor: req.actor,
            session: req.session,
            token: req.token,
            device: req.device,
            client_label: req.client_label.clone(),
            restored_from: req.restored_from,
            parked_from: req.parked_from,
            summary: req.summary,
            committed_at: line.now,
            play_ms: play_ms as u64,
            pinned: false,
        },
        replayed: false,
        blob_created,
    })
}

async fn find_commit_on(
    conn: &mut PgConnection,
    commit_id: CommitId,
) -> Result<Option<RevisionRecord>, SaveError> {
    let sql = format!(
        "SELECT {REVISION_COLUMNS} FROM save_revision r JOIN save_blob b ON b.sha256 = r.blob_sha256 WHERE r.commit_id = $1"
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(commit_id.as_uuid())
        .fetch_optional(&mut *conn)
        .await
        .map_err(unavailable)?
        .map(|row| revision_from_row(&row))
        .transpose()
}

/// A commit refused under the lock: audited outside the rolled-back transaction, so support sees
/// every refusal, not only the successes.
async fn audit_refusal(pool: &PgPool, req: &CommitRequest, error: &SaveError) {
    let detail = serde_json::json!({ "code": error.code(), "expected": req.expected.get(), "commitId": req.commit_id.as_uuid().to_string() });
    if let Ok(mut conn) = pool.acquire().await
        && let Err(e) = audit(
            &mut conn,
            req.profile,
            "commit_refused",
            Some(req.session),
            Some(req.token),
            None,
            detail,
        )
        .await
    {
        tracing::warn!(error = %e, "could not audit a refused commit");
    }
}

fn parked_from_row(row: &PgRow) -> Result<ParkedRecord, SaveError> {
    let corrupt = |what: &str| SaveError::Unavailable(format!("save_parked has an invalid {what}"));
    let reason: String = row.try_get("reason").map_err(unavailable)?;
    let summary: Option<serde_json::Value> = row.try_get("summary").map_err(unavailable)?;
    let parked_at = from_ts(row.try_get("parked_at").map_err(unavailable)?);
    let expires_at = from_ts(row.try_get("expires_at").map_err(unavailable)?);
    let based_on: Option<i64> = row.try_get("based_on_revision").map_err(unavailable)?;
    let session: Option<Uuid> = row.try_get("session_id").map_err(unavailable)?;
    let device: Option<Uuid> = row.try_get("device_id").map_err(unavailable)?;
    Ok(ParkedRecord {
        id: row.try_get("id").map_err(unavailable)?,
        request: ParkRequest {
            profile: ProfileId::from_uuid(row.try_get("profile_id").map_err(unavailable)?)
                .ok_or_else(|| corrupt("profile"))?,
            blob: BlobRef {
                sha256: digest(row, "blob_sha256")?,
                size: row.try_get::<i32, _>("size_bytes").map_err(unavailable)? as u32,
                format: None,
            },
            content: digest(row, "content_sha256")?,
            reason: ParkReason::parse(&reason).ok_or_else(|| corrupt("reason"))?,
            based_on: based_on.map(|r| Revision::new(r as u64)),
            session: session.and_then(PlaySessionId::from_uuid),
            device: device.and_then(DeviceId::from_uuid),
            summary: summary
                .map(serde_json::from_value)
                .transpose()
                .map_err(|_| corrupt("summary"))?,
            detail: row.try_get("detail").map_err(unavailable)?,
            retain_ms: expires_at.since(parked_at),
        },
        parked_at,
        expires_at,
        blob_created: false,
    })
}

impl SaveStore for PgSaveStore {
    async fn ensure_line(&self, profile: ProfileId) -> Result<(), SaveError> {
        sqlx::query(
            "INSERT INTO save_head (profile_id) VALUES ($1) ON CONFLICT (profile_id) DO NOTHING",
        )
        .bind(profile.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if is_foreign_key_violation(&e) {
                SaveError::ProfileNotFound
            } else {
                unavailable(e)
            }
        })?;
        Ok(())
    }

    async fn head(&self, profile: ProfileId) -> Result<HeadView, SaveError> {
        let sql = format!(
            "SELECT {HEAD_COLUMNS}, p.mode AS line_mode, clock_timestamp() AS now, {REVISION_COLUMNS} \
             FROM save_head h JOIN profile p ON p.id = h.profile_id \
             LEFT JOIN save_revision r ON r.profile_id = h.profile_id AND r.revision = h.revision \
             LEFT JOIN save_blob b ON b.sha256 = r.blob_sha256 \
             WHERE h.profile_id = $1"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(profile.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(unavailable)?
            .ok_or(SaveError::ProfileNotFound)?;
        let revision = Revision::new(
            row.try_get::<i64, _>("line_revision")
                .map_err(unavailable)? as u64,
        );
        let current = if revision.is_none() {
            None
        } else {
            Some(revision_from_row(&row)?)
        };
        let lease = lease_from_row(&row)?;
        let now = from_ts(row.try_get("now").map_err(unavailable)?);
        let credited = row.try_get::<i64, _>("line_play_ms").map_err(unavailable)? as u64;
        let mode: String = row.try_get("line_mode").map_err(unavailable)?;
        Ok(HeadView {
            profile,
            mode: mode_of(&mode)?,
            revision,
            current,
            play_ms: lease::play_ms_at(credited, lease.as_ref(), now),
            lease,
            fence_seq: row.try_get::<i64, _>("fence_seq").map_err(unavailable)? as u64,
            reset_revision: Revision::new(
                row.try_get::<i64, _>("reset_revision")
                    .map_err(unavailable)? as u64,
            ),
            now,
        })
    }

    async fn acquire(
        &self,
        profile: ProfileId,
        req: AcquireRequest,
    ) -> Result<LeaseGrant, SaveError> {
        let mut tx = begin(&self.pool).await?;
        let grant = acquire_in(&mut tx, profile, &req).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(grant)
    }

    async fn renew(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
        ttl_ms: u32,
    ) -> Result<Lease, SaveError> {
        let mut tx = begin(&self.pool).await?;
        let renewed = renew_in(&mut tx, profile, session, token, ttl_ms).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(renewed)
    }

    async fn release(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
    ) -> Result<Released, SaveError> {
        let mut tx = begin(&self.pool).await?;
        let outcome = release_in(&mut tx, profile, session, token).await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(outcome)
    }

    async fn commit(&self, req: CommitRequest) -> Result<Committed, SaveError> {
        let mut tx = begin(&self.pool).await?;
        match commit_checked_in(&mut tx, &req, self.eligible).await {
            Ok(done) => {
                tx.commit().await.map_err(unavailable)?;
                Ok(done)
            }
            Err(e) => {
                tx.rollback().await.map_err(unavailable)?;
                if !matches!(e, SaveError::Unavailable(_) | SaveError::ProfileNotFound) {
                    audit_refusal(&self.pool, &req, &e).await;
                }
                Err(e)
            }
        }
    }

    async fn history(
        &self,
        profile: ProfileId,
        before: Option<Revision>,
        limit: u32,
    ) -> Result<Vec<RevisionRecord>, SaveError> {
        self.history_rows(profile, before, limit, false).await
    }

    async fn play_window_start(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
        min_play_ms: u64,
    ) -> Result<Option<RevisionRecord>, SaveError> {
        // Among the readable revisions at or below the bound and not older than the newest rate
        // anchor (a restore is not one, review G2-2): the newest below the play time (on the
        // line's play-time index, review G2-5/G2-12), else the oldest.
        let candidates = format!(
            "FROM save_revision r LEFT JOIN save_blob b ON b.sha256 = r.blob_sha256 \
             WHERE r.profile_id = $1 AND r.revision <= $2 AND {READABLE} \
               AND r.revision >= COALESCE((SELECT max(a.revision) FROM save_revision a \
                 WHERE a.profile_id = $1 AND a.revision <= $2 AND a.kind IN ({RATE_ANCHORS})), 0)"
        );
        let sql = format!(
            "SELECT * FROM ( \
               (SELECT 1 AS pick, {REVISION_COLUMNS} {candidates} AND r.play_ms < $3 \
                 ORDER BY r.play_ms DESC, r.revision DESC LIMIT 1) \
               UNION ALL \
               (SELECT 2 AS pick, {REVISION_COLUMNS} {candidates} ORDER BY r.revision LIMIT 1) \
             ) AS w ORDER BY pick LIMIT 1"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(profile.as_uuid())
            .bind(to_i64(at_or_below.get())?)
            .bind(to_i64(min_play_ms)?)
            .fetch_optional(&self.pool)
            .await
            .map_err(unavailable)?;
        row.as_ref().map(revision_from_row).transpose()
    }

    async fn level_high_water(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
    ) -> Result<Option<i64>, SaveError> {
        let sql = format!(
            "SELECT max((r.summary ->> 'level')::bigint) FROM save_revision r \
             WHERE r.profile_id = $1 AND r.revision <= $2 \
               AND jsonb_typeof(r.summary -> 'level') = 'number' \
               AND r.revision >= COALESCE((SELECT max(a.revision) FROM save_revision a \
                 WHERE a.profile_id = $1 AND a.revision <= $2 \
                   AND a.kind IN ({RATE_ANCHORS}, 'restore')), 0)"
        );
        sqlx::query_scalar::<_, Option<i64>>(sqlx::AssertSqlSafe(sql))
            .bind(profile.as_uuid())
            .bind(to_i64(at_or_below.get())?)
            .fetch_one(&self.pool)
            .await
            .map_err(unavailable)
    }

    // Download and restore read this: a revision whose bundle retention pruned is not found
    // (SAVES_API.md §8), although its row stays for the gates.
    async fn revision(
        &self,
        profile: ProfileId,
        revision: Revision,
    ) -> Result<RevisionRecord, SaveError> {
        let sql = format!(
            "SELECT h.profile_id AS line, {REVISION_COLUMNS} FROM save_head h \
             LEFT JOIN save_revision r ON r.profile_id = h.profile_id AND r.revision = $2 \
               AND r.bundle_pruned_at IS NULL \
             LEFT JOIN save_blob b ON b.sha256 = r.blob_sha256 \
             WHERE h.profile_id = $1"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(profile.as_uuid())
            .bind(to_i64(revision.get())?)
            .fetch_optional(&self.pool)
            .await
            .map_err(unavailable)?
            .ok_or(SaveError::ProfileNotFound)?;
        if row
            .try_get::<Option<i64>, _>("revision")
            .map_err(unavailable)?
            .is_none()
        {
            return Err(SaveError::RevisionNotFound(revision));
        }
        revision_from_row(&row)
    }

    async fn find_commit(&self, commit_id: CommitId) -> Result<Option<RevisionRecord>, SaveError> {
        let mut conn = self.pool.acquire().await.map_err(unavailable)?;
        find_commit_on(&mut conn, commit_id).await
    }

    async fn park(&self, req: ParkRequest) -> Result<ParkedRecord, SaveError> {
        let mut tx = begin(&self.pool).await?;
        // The line's format, for bytes of neither magic (CR-S105 review 4); a profile that is gone
        // parks nothing (the insert below fails).
        let game: Option<String> = sqlx::query_scalar("SELECT game FROM profile WHERE id = $1")
            .bind(req.profile.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(unavailable)?;
        let format = fi_saves::SaveFormat::of_game(game.as_deref().unwrap_or_default());
        let blob_created = insert_blob(&mut tx, &req.blob, format).await?;
        let row = sqlx::query(
            "INSERT INTO save_parked (profile_id, blob_sha256, content_sha256, reason, based_on_revision, session_id, device_id, summary, detail, \
             expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, clock_timestamp() + make_interval(secs => $10::double precision / 1000)) \
             RETURNING id, parked_at, expires_at",
        )
        .bind(req.profile.as_uuid())
        .bind(req.blob.sha256.0.to_vec())
        .bind(req.content.0.to_vec())
        .bind(req.reason.as_str())
        .bind(req.based_on.map(|r| r.get() as i64))
        .bind(req.session.map(|s| s.as_uuid()))
        .bind(req.device.map(|d| d.as_uuid()))
        .bind(req.summary.as_ref().map(summary_json))
        .bind(req.detail.chars().take(500).collect::<String>())
        .bind(req.retain_ms)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| if is_foreign_key_violation(&e) { SaveError::ProfileNotFound } else { unavailable(e) })?;
        let id: Uuid = row.try_get("id").map_err(unavailable)?;
        let detail = serde_json::json!({ "parked": id.to_string(), "reason": req.reason.as_str() });
        audit(
            &mut tx,
            req.profile,
            "parked",
            req.session,
            None,
            req.based_on,
            detail,
        )
        .await?;
        tx.commit().await.map_err(unavailable)?;
        Ok(ParkedRecord {
            id,
            parked_at: from_ts(row.try_get("parked_at").map_err(unavailable)?),
            expires_at: from_ts(row.try_get("expires_at").map_err(unavailable)?),
            request: req,
            blob_created,
        })
    }

    async fn parked(&self, profile: ProfileId) -> Result<Vec<ParkedRecord>, SaveError> {
        let rows = sqlx::query(
            "SELECT s.id, s.profile_id, s.blob_sha256, b.size_bytes, s.content_sha256, s.reason, s.based_on_revision, s.session_id, \
             s.device_id, s.summary, s.detail, s.parked_at, s.expires_at \
             FROM save_parked s JOIN save_blob b ON b.sha256 = s.blob_sha256 \
             WHERE s.profile_id = $1 ORDER BY s.parked_at, s.id",
        )
        .bind(profile.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(unavailable)?;
        rows.iter().map(parked_from_row).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_saves::CommitKind;

    #[test]
    fn rate_anchors_match_the_model() {
        let listed: Vec<&str> = RATE_ANCHORS
            .split(',')
            .map(|k| k.trim().trim_matches('\''))
            .collect();
        let model: Vec<&str> = CommitKind::ALL
            .into_iter()
            .filter(|k| k.is_rate_anchor())
            .map(CommitKind::as_str)
            .collect();
        assert_eq!(listed, model);
        assert!(!listed.contains(&"restore"), "review G2-2");
    }
}
