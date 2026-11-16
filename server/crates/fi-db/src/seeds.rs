//! The Hardcore seed (OD-60, register CR-S20; SAVES_API.md §4.8, CLOUD_SAVES.md §7, migration
//! `20261019170100_save_seed.sql`): one clean snapshot per game, a Hardcore line's revision 1.
//!
//! - [`import_in`] (`fi-cli seed-import`, as the migrator): records a verified bundle whose object
//!   the caller put in the save bucket as the game's active seed, retiring the previous one; the
//!   active seed again changes nothing. The numbers must be readable (fail closed: they are the
//!   gates' base).
//! - [`active_in`]: the game's active seed (`None`: none imported).
//! - [`seed_line_in`]: `POST .../save/seed`, in the caller's transaction. On a line at revision 0 it
//!   commits revision 1 as `fresh_career` (actor `system`) naming the seed's blob (no copy), under
//!   the caller's session lease or a 30 s internal one (only on a line no session holds: it never
//!   replaces a live lease, the caller's own included), re-checking eligibility under the line's
//!   lock (`SAVES.NOT_ELIGIBLE`), then sets each missing half of the RPC gates' baseline of the
//!   profile from the seed's numbers with no recent growth (rates 0). A line whose revision 1 is its fresh
//!   career and nothing else is [`Seeded::Already`] (nothing written, the baseline untouched); a
//!   line with later revisions is [`SeedError::HasRevisions`].
//! - [`profile_baseline_from_in`], [`garage_baseline_from_in`]: a missing half of a Hardcore
//!   profile's RPC baseline from the seed (the RPC path's too: a baseline never comes from a sync).
//!
//! Locks: the line's head (`FOR UPDATE`, as every save write takes it), then the integrity baseline's
//! advisory lock (`integrity::lock_in`, as an RPC sync takes it). Eligibility (OD-52) is checked
//! by the commit under the head's lock.

use crate::identity::{IdentityError, ms};
use crate::integrity::{self, ProfileNumbers, Subject};
use crate::saves;
use fi_domain::{DeviceId, PlaySessionId, ProfileId, UnixMs};
use fi_saves::{
    AcquireMode, AcquireRequest, Actor, BlobRef, CommitId, CommitKind, CommitRequest, Digest,
    FenceToken, LeaseHolder, Revision, RevisionRecord, SaveError, SaveSummary,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// The one kind of seed (the table's CHECK).
pub const HARDCORE: &str = "hardcore";
/// The internal lease of a seeding without the caller's session (a restore's, SAVES_API.md §4.5).
pub const INTERNAL_LEASE_MS: u32 = 30_000;

/// A seed as recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seed {
    /// Row id.
    pub id: Uuid,
    /// `fh2`.
    pub game: String,
    /// [`HARDCORE`].
    pub kind: String,
    /// The bundle in the save bucket.
    pub blob: BlobRef,
    /// Its content digest.
    pub content: Digest,
    /// Its label (16 upper-case hex digits).
    pub label: String,
    /// Its numbers (credits, XP and level always present; cars when the reader counts them).
    pub summary: SaveSummary,
    /// Race winnings: the bundle's `TotalWinnings` at import, 0 when it was unreadable (a
    /// brand-new career has none). Also in `summary.winnings`.
    pub winnings: i64,
    /// When it was imported.
    pub created_at: UnixMs,
}

/// What an import records.
#[derive(Clone, Debug)]
pub struct NewSeed<'a> {
    /// The game (a `game` row).
    pub game: &'a str,
    /// [`HARDCORE`].
    pub kind: &'a str,
    /// The bundle, already put in the bucket under its digest.
    pub blob: BlobRef,
    /// Its content digest (from verification).
    pub content: Digest,
    /// Its label (`Verified::label_hex`).
    pub label: &'a str,
    /// Its numbers (from verification).
    pub summary: SaveSummary,
}

/// What an import did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Imported {
    /// Recorded as the active seed (any previous one retired).
    Created(Seed),
    /// That bundle was already the active seed: nothing changed.
    AlreadyActive(Seed),
}

/// A seeding request (`POST .../save/seed`).
#[derive(Clone, Debug)]
pub struct SeedLine<'a> {
    /// The profile (the caller checked it is the caller's, eligible, of `game`).
    pub profile: ProfileId,
    /// Its game.
    pub game: &'a str,
    /// The `Idempotency-Key`: revision 1's commit id.
    pub commit_id: CommitId,
    /// The caller's play session and fence, when it holds the line's lease.
    pub writer: Option<(PlaySessionId, FenceToken)>,
    /// The caller's install: the internal lease's holder.
    pub device: DeviceId,
    /// Its label.
    pub device_label: &'a str,
}

/// What a seeding did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seeded {
    /// Revision 1 committed and the baseline set.
    Created(Box<RevisionRecord>),
    /// The line's revision 1 is already its fresh career and nothing followed: nothing written.
    Already,
}

/// Why an import or a seeding refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum SeedError {
    /// No active seed for the game (`SERVICE.UNAVAILABLE`).
    #[error("no active seed for this game")]
    NoSeed,
    /// The line has revisions after its start (`SAVES.REVISION_CONFLICT {expected: 0, head}`).
    #[error("the line already has revisions (head {head})")]
    HasRevisions {
        /// The head revision.
        head: u64,
    },
    /// An import whose credits, XP or level cannot be read, or an unknown kind.
    #[error("a seed needs readable credits, XP and level")]
    Unreadable,
    /// A save rule (lease, CAS, the line's mode) or storage failure.
    #[error(transparent)]
    Save(#[from] SaveError),
    /// The baseline's writes.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// Any other database failure.
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

const SEED_COLUMNS: &str = "id, game, kind, blob_sha256, content_sha256, size_bytes, label, \
    credits, xp, level, winnings, cars, created_at";

fn digest(row: &PgRow, column: &str) -> Result<Digest, SeedError> {
    let bytes: Vec<u8> = row.try_get(column)?;
    let arr: [u8; 32] = bytes.try_into().map_err(|_| SeedError::Unreadable)?;
    Ok(Digest(arr))
}

fn seed_from_row(row: &PgRow) -> Result<Seed, SeedError> {
    Ok(Seed {
        id: row.try_get("id")?,
        game: row.try_get("game")?,
        kind: row.try_get("kind")?,
        blob: BlobRef {
            sha256: digest(row, "blob_sha256")?,
            size: row.try_get::<i32, _>("size_bytes")? as u32,
            format: None,
        },
        content: digest(row, "content_sha256")?,
        label: row.try_get("label")?,
        summary: SaveSummary {
            credits: Some(row.try_get("credits")?),
            xp: Some(row.try_get("xp")?),
            level: Some(row.try_get("level")?),
            cars: row.try_get::<Option<i32>, _>("cars")?.map(i64::from),
            // The seed's lifetime winnings (a brand-new career's 0), so a line's revision 1 carries
            // them and the guardrails' credits conservation applies from the first upload.
            winnings: Some(row.try_get("winnings")?),
            // The seed's skill state is not kept: the token rule checks an upload's own state.
            ..SaveSummary::default()
        },
        winnings: row.try_get("winnings")?,
        created_at: ms(row.try_get::<OffsetDateTime, _>("created_at")?),
    })
}

/// The game's active seed of `kind`.
pub async fn active_in(
    conn: &mut PgConnection,
    game: &str,
    kind: &str,
) -> Result<Option<Seed>, SeedError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SEED_COLUMNS} FROM save_seed WHERE game = $1 AND kind = $2 AND active"
    )))
    .bind(game)
    .bind(kind)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(seed_from_row).transpose()
}

/// The seed (active or retired) whose bundle is `blob`: the seed a line's `fresh_career` revision 1
/// was committed from (`seed_line_in`), so a line is judged against its own seed after a rotation.
pub async fn by_blob_in(
    conn: &mut PgConnection,
    game: &str,
    blob: &[u8],
) -> Result<Option<Seed>, SeedError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SEED_COLUMNS} FROM save_seed WHERE game = $1 AND blob_sha256 = $2 \
         ORDER BY created_at DESC LIMIT 1"
    )))
    .bind(game)
    .bind(blob)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref().map(seed_from_row).transpose()
}

/// Records `new` as the game's active seed of its kind (as the migrator), retiring the previous
/// one; the active seed again changes nothing. The blob row is registered (or its collection mark
/// cleared) in the same transaction; the caller put the object first and puts it again after the
/// commit (a collection may have deleted it in between, CLOUD_SAVES.md §6 step 4).
pub async fn import_in(conn: &mut PgConnection, new: &NewSeed<'_>) -> Result<Imported, SeedError> {
    let number = |v: Option<i64>| v.filter(|n| *n >= 0).ok_or(SeedError::Unreadable);
    let (credits, xp, level) = (
        number(new.summary.credits)?,
        number(new.summary.xp)?,
        number(new.summary.level)?,
    );
    if new.kind != HARDCORE {
        return Err(SeedError::Unreadable);
    }
    saves::insert_blob(conn, &new.blob, fi_saves::SaveFormat::of_game(new.game)).await?;
    // One import at a time per game and kind: the partial unique index would refuse a second
    // active row anyway; the advisory lock makes a concurrent import wait instead of fail.
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('fi.save_seed'), hashtext($1 || ':' || $2))",
    )
    .bind(new.game)
    .bind(new.kind)
    .execute(&mut *conn)
    .await?;
    if let Some(active) = active_in(conn, new.game, new.kind).await?
        && active.blob.sha256 == new.blob.sha256
    {
        return Ok(Imported::AlreadyActive(active));
    }
    sqlx::query(
        "UPDATE save_seed SET active = false, retired_at = clock_timestamp() \
         WHERE game = $1 AND kind = $2 AND active",
    )
    .bind(new.game)
    .bind(new.kind)
    .execute(&mut *conn)
    .await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO save_seed (game, kind, blob_sha256, content_sha256, size_bytes, label, credits, xp, \
           level, winnings, cars) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING {SEED_COLUMNS}"
    )))
    .bind(new.game)
    .bind(new.kind)
    .bind(new.blob.sha256.0.to_vec())
    .bind(new.content.0.to_vec())
    .bind(new.blob.size as i32)
    .bind(new.label)
    .bind(credits)
    .bind(xp)
    .bind(level)
    // `TotalWinnings` when the bundle has it readable (OD-62), else 0 (a brand-new career's).
    .bind(new.summary.winnings.filter(|w| *w >= 0).unwrap_or(0))
    .bind(
        new.summary
            .cars
            .filter(|c| *c >= 0)
            .map(|c| i32::try_from(c).unwrap_or(i32::MAX)),
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok(Imported::Created(seed_from_row(&row)?))
}

/// Imports the synthetic starter (`fi_saves::synthetic::starter`, made-up bytes) as `game`'s
/// seed: what local environments and tests use until a real snapshot is imported (`fi-cli
/// seed-import --synthetic`, which also puts the object). Every caller imports the same bytes, so
/// concurrent suites on one database agree (the active seed again changes nothing).
pub async fn import_synthetic_in(
    conn: &mut PgConnection,
    game: &str,
) -> Result<Imported, SeedError> {
    let bytes = fi_saves::synthetic::starter();
    let v = fi_saves::bundle::verify(bytes).map_err(|_| SeedError::Unreadable)?;
    import_in(
        conn,
        &NewSeed {
            game,
            kind: HARDCORE,
            blob: BlobRef {
                sha256: Digest::of(bytes),
                size: bytes.len() as u32,
                format: fi_saves::SaveFormat::of_magic(bytes),
            },
            content: v.content,
            label: &v.label_hex(),
            summary: v.summary,
        },
    )
    .await
}

/// `POST .../save/seed` in the caller's transaction (see the module).
pub async fn seed_line_in(
    conn: &mut PgConnection,
    req: &SeedLine<'_>,
) -> Result<Seeded, SeedError> {
    let seed = active_in(conn, req.game, HARDCORE)
        .await?
        .ok_or(SeedError::NoSeed)?;
    sqlx::query(
        "INSERT INTO save_head (profile_id) VALUES ($1) ON CONFLICT (profile_id) DO NOTHING",
    )
    .bind(req.profile.as_uuid())
    .execute(&mut *conn)
    .await?;
    // The head and, when a session's lease on it is live, its holder.
    type Head = (i64, Option<Uuid>, Option<String>, Option<OffsetDateTime>);
    let (head, held_by, held_label, held_until): Head = sqlx::query_as(
        "SELECT revision, \
           CASE WHEN lease_expires_at > clock_timestamp() THEN lease_device_id END, \
           CASE WHEN lease_expires_at > clock_timestamp() THEN lease_device_label END, \
           CASE WHEN lease_expires_at > clock_timestamp() THEN lease_expires_at END \
         FROM save_head WHERE profile_id = $1 FOR UPDATE",
    )
    .bind(req.profile.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    if head > 0 {
        let first: Option<String> = sqlx::query_scalar(
            "SELECT kind FROM save_revision WHERE profile_id = $1 AND revision = 1",
        )
        .bind(req.profile.as_uuid())
        .fetch_optional(&mut *conn)
        .await?;
        return if head == 1 && first.as_deref() == Some(CommitKind::FreshCareer.as_str()) {
            Ok(Seeded::Already)
        } else {
            Err(SeedError::HasRevisions { head: head as u64 })
        };
    }
    // OD-52 under the head's lock, before any lease: an access that ended or a mode change (which
    // retires the line) since the caller's check is a refusal, not a lease on a retired line.
    let account: Uuid = sqlx::query_scalar("SELECT account_id FROM profile WHERE id = $1")
        .bind(req.profile.as_uuid())
        .fetch_one(&mut *conn)
        .await?;
    if let Some((tier, mode)) = crate::cloud::refusal_in(conn, account, req.profile).await? {
        return Err(SeedError::Save(SaveError::NotEligible {
            tier: tier.as_str().to_owned(),
            mode,
        }));
    }
    let (session, token, internal) = match req.writer {
        Some((session, token)) => (session, token, false),
        None => {
            // Without the caller's session the line must be free: the lease rules would let the
            // same install replace its own running session's lease, leaving that session with a
            // stale fence. A live lease is LEASE_HELD (send its session headers instead).
            if let (Some(device), Some(label), Some(until)) = (held_by, held_label, held_until) {
                return Err(SeedError::Save(SaveError::LeaseHeld {
                    device: DeviceId::from_uuid(device).ok_or(SeedError::Unreadable)?,
                    device_label: label,
                    expires_at: ms(until),
                }));
            }
            let session = PlaySessionId::new();
            let grant = saves::acquire_in(
                conn,
                req.profile,
                &AcquireRequest {
                    holder: LeaseHolder {
                        session,
                        device: req.device,
                        device_label: req.device_label.to_owned(),
                    },
                    ttl_ms: INTERNAL_LEASE_MS,
                    mode: AcquireMode::Normal,
                },
            )
            .await?;
            (session, grant.lease.token, true)
        }
    };
    let committed = saves::commit_checked_in(
        conn,
        &CommitRequest {
            profile: req.profile,
            session,
            token,
            expected: Revision::NONE,
            commit_id: req.commit_id,
            blob: seed.blob,
            content: seed.content,
            kind: CommitKind::FreshCareer,
            actor: Actor::System,
            device: None,
            client_label: Some(seed.label.clone()),
            restored_from: None,
            summary: seed.summary,
            gated_base: None,
            parked_from: None,
            guard_input: None,
            seed_source: None,
        },
        true,
    )
    .await?;
    if internal {
        saves::release_in(conn, req.profile, session, token).await?;
    }
    // The RPC gates start from the seed's numbers, no recent growth (GAME_PLATFORM.md §3a); a
    // half the RPC path already took from the seed is kept.
    integrity::lock_in(conn, req.game, req.profile.as_uuid()).await?;
    let subject = Subject {
        game: req.game,
        profile_id: req.profile.as_uuid(),
        mode: HARDCORE,
    };
    profile_baseline_from_in(conn, subject, &seed).await?;
    garage_baseline_from_in(conn, subject, &seed).await?;
    Ok(Seeded::Created(Box::new(committed.record)))
}

/// The numbers the RPC gates hold for `seed`: its credits, XP and level (an import refuses a seed
/// without them), its winnings (0), no top speed.
fn seed_numbers(seed: &Seed) -> ProfileNumbers {
    ProfileNumbers {
        credits: seed.summary.credits.unwrap_or(0),
        xp: seed.summary.xp.unwrap_or(0),
        level: seed.summary.level.unwrap_or(0),
        winnings: seed.winnings,
        top_speed_mps: 0.0,
    }
}

/// Sets the profile half of a Hardcore profile's RPC baseline from `seed` with no recent growth
/// when that half is missing (OD-60: never from a sync); an existing half is kept, and so is an
/// open refusal episode. Returns whether it wrote. The caller holds [`integrity::lock_in`].
pub async fn profile_baseline_from_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    seed: &Seed,
) -> Result<bool, IdentityError> {
    profile_baseline_of_in(conn, subject, &seed_numbers(seed)).await
}

/// [`profile_baseline_from_in`] from `n` (a seed's numbers, or a verified `fresh_start`'s,
/// CR-S51): written only when the half is missing.
pub async fn profile_baseline_of_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    n: &ProfileNumbers,
) -> Result<bool, IdentityError> {
    let wrote = sqlx::query(
        "INSERT INTO integrity_baseline \
           (game, profile_id, mode, credits, xp, level, winnings, top_speed_mps, synced_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, clock_timestamp()) \
         ON CONFLICT (game, profile_id, mode) DO UPDATE SET \
           credits = EXCLUDED.credits, xp = EXCLUDED.xp, level = EXCLUDED.level, \
           winnings = EXCLUDED.winnings, top_speed_mps = EXCLUDED.top_speed_mps, \
           synced_at = EXCLUDED.synced_at, rate_credits = NULL, rate_unexplained = NULL, \
           rate_winnings = NULL, rate_xp = NULL, rate_levels = NULL, rate_day_unexplained = NULL, \
           rate_day_winnings = NULL, rate_day_xp = NULL, rate_day_levels = NULL, level_high = NULL, \
           judged_level_deviation = NULL, judged_drop_xp = NULL, judged_drop_level = NULL, \
           updated_at = clock_timestamp() \
         WHERE integrity_baseline.synced_at IS NULL",
    )
    .bind(subject.game)
    .bind(subject.profile_id)
    .bind(subject.mode)
    .bind(n.credits)
    .bind(n.xp)
    .bind(n.level)
    .bind(n.winnings)
    .bind(n.top_speed_mps)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(wrote > 0)
}

/// [`profile_baseline_from_in`] for the garage half: the seed's cars (0 while the reader counts
/// none), no recent growth.
pub async fn garage_baseline_from_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    seed: &Seed,
) -> Result<bool, IdentityError> {
    garage_baseline_of_in(conn, subject, seed.summary.cars.unwrap_or(0)).await
}

/// [`garage_baseline_from_in`] from a garage of `cars` (a seed's, or a verified `fresh_start`'s,
/// CR-S51): written only when the half is missing.
pub async fn garage_baseline_of_in(
    conn: &mut PgConnection,
    subject: Subject<'_>,
    cars: i64,
) -> Result<bool, IdentityError> {
    let cars = i32::try_from(cars.max(0)).unwrap_or(i32::MAX);
    let wrote = sqlx::query(
        "INSERT INTO integrity_baseline (game, profile_id, mode, garage_cars, tables_synced_at) \
         VALUES ($1, $2, $3, $4, clock_timestamp()) \
         ON CONFLICT (game, profile_id, mode) DO UPDATE SET \
           garage_cars = EXCLUDED.garage_cars, tables_synced_at = EXCLUDED.tables_synced_at, \
           rate_cars = NULL, updated_at = clock_timestamp() \
         WHERE integrity_baseline.tables_synced_at IS NULL",
    )
    .bind(subject.game)
    .bind(subject.profile_id)
    .bind(subject.mode)
    .bind(cars)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(wrote > 0)
}
