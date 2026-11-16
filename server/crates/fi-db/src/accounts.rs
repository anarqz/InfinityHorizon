//! Accounts, identities, profiles and devices (ACCOUNTS.md §2, §3, §4.1, §5, §8).
//!
//! Functions over the caller's connection (`*_in`, inside its transaction) plus thin pool
//! wrappers for reads. The order of row locks is always the account row first
//! ([`lock_account_in`]), then anything under it, so two writes to one account serialize and no two
//! transactions wait for each other in opposite order.

use crate::identity::{IdentityClaim, IdentityError, is_unique_violation, ms};
use fi_domain::{ProfileMode, UnixMs};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Live profiles per account per mode for a new profile (all tiers, CR-S98, CAREERS.md §2).
pub const MAX_PROFILES_PER_MODE: i64 = 1;
/// Deletion grace (ACCOUNTS.md §8).
pub const DELETION_GRACE_DAYS: i64 = 14;

/// A launcher install as it introduces itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDevice {
    /// Install id.
    pub device_id: Uuid,
    /// PC name (1-64 characters).
    pub label: String,
    /// `windows`, `proton`, `linux`.
    pub platform: String,
    /// HMAC-SHA256 with the pepper of the launcher's `Device.fingerprint` (CR-S44), when it sent
    /// one: stored on the binding, never returned.
    pub fingerprint_hmac: Option<[u8; 32]>,
}

/// A profile to create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewProfile {
    /// `fh2`.
    pub game: String,
    /// The plain gamertag (validated by the caller: `fi_domain::gamertag::valid_name`; reserved
    /// names are refused here).
    pub name: String,
    /// Hardcore or Noob.
    pub mode: ProfileMode,
}

/// A profile as the API shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileRow {
    /// Id.
    pub profile_id: Uuid,
    /// Account.
    pub account_id: Uuid,
    /// Game code.
    pub game: String,
    /// XUID.
    pub xuid: u64,
    /// The plain gamertag (OD-120).
    pub name: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// `active` | `deletion_pending`.
    pub status: String,
    /// `created` | `beta_import`.
    pub origin: String,
    /// Created.
    pub created_at: UnixMs,
    /// `row_version`.
    pub row_version: i64,
    /// End of the last play session.
    pub last_played_at: Option<UnixMs>,
    /// The OD-120 clash backfill's notice while the free rename is unused: the name before it
    /// and when.
    pub rename_notice: Option<(String, UnixMs)>,
    /// Listed on the play dashboard (CR-S83, OD-168 follow-up DB-Q4; default `true`).
    pub web_listed: bool,
}

impl ProfileRow {
    /// The gamertag: the plain name (OD-120).
    pub fn gamertag(&self) -> String {
        self.name.clone()
    }

    /// The handle: the plain name too (OD-120).
    pub fn handle(&self) -> String {
        self.name.clone()
    }
}

const PROFILE_COLUMNS: &str = "p.id, p.account_id, p.game, p.xuid, p.name, p.mode, p.status, \
    p.origin, p.created_at, p.row_version, p.rename_notice_from, p.rename_notice_at, p.web_listed, \
    (SELECT max(s.ended_at) FROM play_session s WHERE s.profile_id = p.id) AS last_played_at";

fn profile_from_row(r: &PgRow) -> Result<ProfileRow, sqlx::Error> {
    Ok(ProfileRow {
        profile_id: r.try_get("id")?,
        account_id: r.try_get("account_id")?,
        game: r.try_get("game")?,
        xuid: r.try_get::<i64, _>("xuid")? as u64,
        name: r.try_get("name")?,
        mode: r.try_get("mode")?,
        status: r.try_get("status")?,
        origin: r.try_get("origin")?,
        created_at: ms(r.try_get("created_at")?),
        row_version: r.try_get("row_version")?,
        last_played_at: r
            .try_get::<Option<OffsetDateTime>, _>("last_played_at")?
            .map(ms),
        rename_notice: match (
            r.try_get::<Option<String>, _>("rename_notice_from")?,
            r.try_get::<Option<OffsetDateTime>, _>("rename_notice_at")?,
        ) {
            (Some(from), Some(at)) => Some((from, ms(at))),
            _ => None,
        },
        web_listed: r.try_get("web_listed")?,
    })
}

/// A bound device as `/me` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceRow {
    /// Binding id (the refresh family's `device_binding_id`).
    pub binding_id: Uuid,
    /// Install id.
    pub device_id: Uuid,
    /// PC name.
    pub label: String,
    /// Platform.
    pub platform: String,
    /// Bound.
    pub bound_at: UnixMs,
    /// Last sign-in or refresh from it.
    pub last_seen_at: UnixMs,
}

/// The account state every authenticated request checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountState {
    /// Id.
    pub account_id: Uuid,
    /// `active` | `deletion_pending` | `deleted`.
    pub status: String,
    /// `player` | `moderator` | `admin`.
    pub role: String,
    /// A ban is in force: `banned_at` is set and `banned_until` is unset or still ahead.
    pub banned: bool,
    /// A ban in force that is a block pending review the player is told about (`account.ban_review`,
    /// CR-S52: `seed_from_profile`); `None` for an ordinary ban or none.
    pub ban_review: Option<String>,
    /// Pending deletion: `(requested_at, purge_at)`.
    pub deletion: Option<(UnixMs, UnixMs)>,
    /// `beta_import` | `registered`.
    pub origin: String,
}

fn state_from_row(r: &PgRow) -> Result<AccountState, sqlx::Error> {
    let requested: Option<OffsetDateTime> = r.try_get("deletion_requested_at")?;
    let due: Option<OffsetDateTime> = r.try_get("deletion_due_at")?;
    let status: String = r.try_get("status")?;
    Ok(AccountState {
        account_id: r.try_get("id")?,
        deletion: match (status.as_str(), requested, due) {
            ("deletion_pending", Some(a), Some(b)) => Some((ms(a), ms(b))),
            _ => None,
        },
        status,
        role: r.try_get("role")?,
        banned: r.try_get("banned")?,
        ban_review: r.try_get("ban_review")?,
        origin: r.try_get("origin")?,
    })
}

const ACCOUNT_COLUMNS: &str = "id, status, role, \
     (banned_at IS NOT NULL AND (banned_until IS NULL OR banned_until > clock_timestamp())) AS banned, \
     CASE WHEN banned_at IS NOT NULL AND (banned_until IS NULL OR banned_until > clock_timestamp()) \
       THEN ban_review END AS ban_review, \
     deletion_requested_at, deletion_due_at, origin";

/// Locks the account row (`FOR UPDATE`): the first lock of every account write.
pub async fn lock_account_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<AccountState, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = $1 FOR UPDATE"
    )))
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(IdentityError::NotFound)?;
    Ok(state_from_row(&row)?)
}

/// The account's state, unlocked.
pub async fn account_state(
    pool: &PgPool,
    account_id: Uuid,
) -> Result<Option<AccountState>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = $1"
    )))
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref()
        .map(state_from_row)
        .transpose()
        .map_err(Into::into)
}

/// [`account_state`] on the caller's connection.
pub async fn account_state_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<AccountState>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = $1"
    )))
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref()
        .map(state_from_row)
        .transpose()
        .map_err(Into::into)
}

/// The account mapped to an Identity Platform uid, and records this sign-in (provider, e-mail).
pub async fn find_by_uid_in(
    conn: &mut PgConnection,
    claim: &IdentityClaim,
) -> Result<Option<AccountState>, IdentityError> {
    let row = sqlx::query(
        "UPDATE account_identity SET last_sign_in_at = clock_timestamp(), \
           provider_ids = CASE WHEN $2 = ANY (provider_ids) OR cardinality(provider_ids) >= 16 THEN provider_ids ELSE provider_ids || $2 END, \
           email_lower = COALESCE($3, email_lower), email_verified = email_verified OR $4 \
         WHERE idp_uid = $1 RETURNING account_id",
    )
    .bind(&claim.idp_uid)
    .bind(&claim.provider_id)
    .bind(&claim.email_lower)
    .bind(claim.email_verified)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let account_id: Uuid = row.try_get("account_id")?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = $1"
    )))
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Some(state_from_row(&row)?))
}

/// The uid mapped to `account_id`, if any.
pub async fn uid_of_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Option<String>, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT idp_uid FROM account_identity WHERE account_id = $1")
            .bind(account_id)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// Maps `claim`'s uid to `account_id` (legacy link, account creation). A uid already mapped, or an
/// account that already has an identity, is [`IdentityError::AlreadyLinked`].
pub async fn link_identity_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    claim: &IdentityClaim,
) -> Result<(), IdentityError> {
    let mut sp = sqlx::Connection::begin(&mut *conn).await?;
    let inserted = sqlx::query(
        "INSERT INTO account_identity (account_id, idp_uid, provider_ids, email_lower, email_verified) \
         VALUES ($1, $2, ARRAY[$3], $4, $5)",
    )
    .bind(account_id)
    .bind(&claim.idp_uid)
    .bind(&claim.provider_id)
    .bind(&claim.email_lower)
    .bind(claim.email_verified)
    .execute(&mut *sp)
    .await;
    match inserted {
        Ok(_) => {
            sp.commit().await?;
            Ok(())
        }
        Err(e)
            if is_unique_violation(&e, "account_identity_uid_key")
                || is_unique_violation(&e, "account_identity_pkey") =>
        {
            sp.rollback().await?;
            Err(IdentityError::AlreadyLinked)
        }
        Err(e) => Err(e.into()),
    }
}

/// Creates an account for a verified identity (`registered`), maps the uid, and (launcher) its
/// first profile with an empty save line. Portal accounts have no profile (the launcher adds them).
pub async fn create_account_in(
    conn: &mut PgConnection,
    claim: &IdentityClaim,
    locale: Option<&str>,
    first_profile: Option<&NewProfile>,
) -> Result<(Uuid, Option<ProfileRow>), IdentityError> {
    let account_id: Uuid = sqlx::query_scalar(
        "INSERT INTO account (origin, locale) VALUES ('registered', $1) RETURNING id",
    )
    .bind(locale)
    .fetch_one(&mut *conn)
    .await?;
    link_identity_in(conn, account_id, claim).await?;
    let profile = match first_profile {
        Some(p) => Some(insert_profile_in(conn, account_id, p).await?),
        None => None,
    };
    Ok((account_id, profile))
}

/// Inserts a profile under its plain name (OD-120) and its empty save line (revision 0): a name
/// taken in any game (case-insensitive, CR-S98) or reserved is `NameUnavailable`. Hardcore's fresh
/// career (revision 1) is SER-2.3. The caller holds the account lock.
///
/// CR-S98 (CAREERS.md §1): the row is the profile's first career; the insert trigger
/// (`fi_career_insert`) creates its `player_profile` with the same id, a new XUID's.
pub async fn insert_profile_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    profile: &NewProfile,
) -> Result<ProfileRow, IdentityError> {
    if fi_domain::gamertag::is_reserved(&profile.name) {
        return Err(IdentityError::NameUnavailable);
    }
    let mut sp = sqlx::Connection::begin(&mut *conn).await?;
    let inserted = sqlx::query(
        "INSERT INTO profile (account_id, game, xuid, name, mode, origin) \
         VALUES ($1, $2, nextval('profile_xuid_seq'), $3, $4, 'created') RETURNING id",
    )
    .bind(account_id)
    .bind(&profile.game)
    .bind(&profile.name)
    .bind(profile.mode.as_str())
    .fetch_one(&mut *sp)
    .await;
    let id: Uuid = match inserted {
        Ok(row) => row.try_get("id")?,
        Err(e) if is_name_taken(&e) => {
            sp.rollback().await?;
            return Err(IdentityError::NameUnavailable);
        }
        Err(e) => return Err(e.into()),
    };
    sqlx::query("INSERT INTO save_head (profile_id) VALUES ($1)")
        .bind(id)
        .execute(&mut *sp)
        .await?;
    sp.commit().await?;
    profile_in(conn, id).await?.ok_or(IdentityError::NotFound)
}

/// A name already held by a live profile (`player_profile_name_key`, CR-S98) or career (the
/// per-game `profile_name_key`).
pub fn is_name_taken(e: &sqlx::Error) -> bool {
    is_unique_violation(e, "player_profile_name_key") || is_unique_violation(e, "profile_name_key")
}

/// The games of the account's live careers of the profile `xuid`, at most two (CR-S98: a route
/// that names no game is refused once there are two, CAREERS.md §6.1).
pub async fn career_games_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
) -> Result<Vec<String>, IdentityError> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(Vec::new());
    };
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT game FROM profile \
         WHERE account_id = $1 AND xuid = $2 AND status <> 'deleted' ORDER BY game LIMIT 2",
    )
    .bind(account_id)
    .bind(xuid)
    .fetch_all(&mut *conn)
    .await?)
}

/// Locks the account row, then its live profile `xuid` (`FOR NO KEY UPDATE`, the order of every
/// profile write), in the caller's transaction (CR-S98: a write that names no game resolves its
/// career under these locks). A missing profile locks nothing more; the write then answers it.
pub async fn lock_profile_of_xuid_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT 1 FROM account WHERE id = $1 FOR NO KEY UPDATE")
        .bind(account_id)
        .execute(&mut *conn)
        .await?;
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(());
    };
    sqlx::query(
        "SELECT 1 FROM player_profile WHERE account_id = $1 AND xuid = $2 AND status <> 'deleted' \
         FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .bind(xuid)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Why a career is not created (`POST /launcher/v1/players/{xuid}/careers`, CAREERS.md §3).
#[derive(Debug, thiserror::Error)]
pub enum CareerError {
    /// No live profile `xuid` of the account (`ACCOUNTS.NOT_FOUND`).
    #[error("not found")]
    NotFound,
    /// The account or the profile is pending deletion (`ACCOUNTS.DELETION_PENDING`).
    #[error("pending deletion")]
    DeletionPending,
    /// The profile has a career of the game, live (`ACCOUNTS.CAREER_EXISTS`).
    #[error("career exists")]
    Exists,
    /// The game does not allow the profile's mode (`REQUEST.INVALID`, `profile.mode`).
    #[error("mode not allowed")]
    ModeNotAllowed,
    /// The account is banned (`ACCOUNTS.BANNED {review?}`).
    #[error("banned")]
    Banned {
        /// `account.ban_review` while the ban is in force.
        review: Option<String>,
    },
    /// Database trouble.
    #[error(transparent)]
    Db(#[from] IdentityError),
}

impl From<sqlx::Error> for CareerError {
    fn from(e: sqlx::Error) -> Self {
        CareerError::Db(e.into())
    }
}

/// `POST /players/{xuid}/careers` (CAREERS.md §3): the account's live profile `xuid` gets an empty
/// career of `game` (known and open: the caller checks) and its empty save line, under the account
/// lock. The career takes a new id; its XUID, name and mode are the profile's. A deleted career of the
/// game stays a tombstone and does not count: the game can start again (CR-S98 review).
pub async fn add_career_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    game: &str,
) -> Result<ProfileRow, CareerError> {
    let state = lock_account_in(conn, account_id).await?;
    match state.status.as_str() {
        "active" => {}
        "deletion_pending" => return Err(CareerError::DeletionPending),
        _ => return Err(CareerError::NotFound),
    }
    // CAREERS.md §6.12: a banned account (a block pending review included) starts no career.
    if state.banned {
        return Err(CareerError::Banned {
            review: state.ban_review,
        });
    }
    let xuid = i64::try_from(xuid).map_err(|_| CareerError::NotFound)?;
    let parent = sqlx::query(
        "SELECT id, mode, status FROM player_profile \
         WHERE account_id = $1 AND xuid = $2 AND status <> 'deleted' FOR NO KEY UPDATE",
    )
    .bind(account_id)
    .bind(xuid)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(CareerError::NotFound)?;
    if parent.try_get::<String, _>("status")? != "active" {
        return Err(CareerError::DeletionPending);
    }
    let mode: ProfileMode = parent
        .try_get::<String, _>("mode")?
        .parse()
        .map_err(|_| CareerError::NotFound)?;
    if !fi_domain::GameRules::of(game).allows(mode) {
        return Err(CareerError::ModeNotAllowed);
    }
    insert_career_in(conn, parent.try_get("id")?, game).await
}

/// The profile `parent`'s empty career of `game` and its empty save line (the caller holds the
/// account lock and checked the game and the mode); a live one already there is
/// [`CareerError::Exists`].
async fn insert_career_in(
    conn: &mut PgConnection,
    parent: Uuid,
    game: &str,
) -> Result<ProfileRow, CareerError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM profile WHERE player_profile_id = $1 AND game = $2 \
           AND status <> 'deleted')",
    )
    .bind(parent)
    .bind(game)
    .fetch_one(&mut *conn)
    .await?;
    if exists {
        return Err(CareerError::Exists);
    }
    let mut sp = sqlx::Connection::begin(&mut *conn).await?;
    let inserted = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO profile (account_id, game, xuid, name, mode, origin, player_profile_id) \
         SELECT p.account_id, $2, p.xuid, p.name, p.mode, 'created', p.id FROM player_profile p \
          WHERE p.id = $1 RETURNING id",
    )
    .bind(parent)
    .bind(game)
    .fetch_one(&mut *sp)
    .await;
    let id = match inserted {
        Ok(id) => id,
        // A concurrent request created it first (the career's name is the profile's, so the
        // per-game name key may answer before the others).
        Err(e)
            if is_unique_violation(&e, "profile_one_career_per_game")
                || is_unique_violation(&e, "profile_game_xuid_key")
                || is_unique_violation(&e, "profile_name_key") =>
        {
            sp.rollback().await?;
            return Err(CareerError::Exists);
        }
        Err(e) => return Err(e.into()),
    };
    sqlx::query("INSERT INTO save_head (profile_id) VALUES ($1)")
        .bind(id)
        .execute(&mut *sp)
        .await?;
    sp.commit().await?;
    profile_in(conn, id)
        .await?
        .ok_or(CareerError::Db(IdentityError::NotFound))
}

/// One profile by id.
pub async fn profile_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
) -> Result<Option<ProfileRow>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {PROFILE_COLUMNS} FROM profile p WHERE p.id = $1 AND p.status <> 'deleted'"
    )))
    .bind(profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.as_ref()
        .map(profile_from_row)
        .transpose()
        .map_err(Into::into)
}

/// The account's live profiles (not erased), oldest first.
pub async fn profiles_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Vec<ProfileRow>, IdentityError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {PROFILE_COLUMNS} FROM profile p WHERE p.account_id = $1 AND p.status <> 'deleted' ORDER BY p.created_at, p.id"
    )))
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(profile_from_row)
        .collect::<Result<_, _>>()
        .map_err(Into::into)
}

/// `POST /profiles`: another profile and its first career, under the account lock: at most one
/// live profile per mode (CR-S98, OD-207; CAREERS.md §2). Accounts that held more before keep
/// them (`fi-cli profiles duplicates`), and take no new one of that mode.
pub async fn add_profile_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    profile: &NewProfile,
) -> Result<ProfileRow, IdentityError> {
    let state = lock_account_in(conn, account_id).await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM player_profile WHERE account_id = $1 AND mode = $2 AND status <> 'deleted'",
    )
    .bind(account_id)
    .bind(profile.mode.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if count >= MAX_PROFILES_PER_MODE {
        // CAREERS.md §6.6: a local profile registering as a career. The account's live profile of
        // the mode with this name (case-insensitive) and no career of the game gets that career;
        // another name stays the limit.
        let same: Option<Uuid> = sqlx::query_scalar(
            "SELECT p.id FROM player_profile p \
             WHERE p.account_id = $1 AND p.mode = $2 AND p.status = 'active' \
               AND lower(p.name) = lower($3) \
               AND NOT EXISTS (SELECT 1 FROM profile c WHERE c.player_profile_id = p.id AND c.game = $4 \
                   AND c.status <> 'deleted') \
             ORDER BY p.created_at, p.id LIMIT 1",
        )
        .bind(account_id)
        .bind(profile.mode.as_str())
        .bind(&profile.name)
        .bind(&profile.game)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(parent) = same else {
            return Err(IdentityError::ProfileLimit);
        };
        // The career route's refusal (CAREERS.md §6.12): a banned account starts no career.
        if state.banned {
            return Err(IdentityError::Banned {
                review: state.ban_review,
            });
        }
        return insert_career_in(conn, parent, &profile.game)
            .await
            .map_err(|e| match e {
                CareerError::Db(e) => e,
                _ => IdentityError::ProfileLimit,
            });
    }
    insert_profile_in(conn, account_id, profile).await
}

/// One career of a [`PlayerRow`] (CAREERS.md §3 `Career`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CareerRow {
    /// The game.
    pub game: String,
    /// The career's id (its `profile` row).
    pub career_id: Uuid,
    /// Created.
    pub created_at: UnixMs,
    /// End of the career's last play session.
    pub last_played_at: Option<UnixMs>,
    /// The save line's head: `(revision, committed_at)`, when it has a revision.
    pub head: Option<(i64, UnixMs)>,
    /// Its `profile_progress` row, when it has one.
    pub progress: Option<CareerProgressRow>,
}

/// A career's progress projection (`profile_progress`, CAREERS.md §6.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CareerProgressRow {
    /// Level.
    pub level: Option<i32>,
    /// Credits.
    pub credits: Option<i64>,
    /// Cars owned.
    pub cars: Option<i32>,
    /// Seconds played.
    pub play_seconds: i64,
    /// When the projection last changed.
    pub synced_at: UnixMs,
}

/// A profile above its careers, as `/me` `players` shows it (CR-S98).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerRow {
    /// The profile's id.
    pub profile_id: Uuid,
    /// The gamertag.
    pub name: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// XUID.
    pub xuid: u64,
    /// `active` | `deletion_pending`.
    pub status: String,
    /// Created.
    pub created_at: UnixMs,
    /// The oldest live career's OD-120 clash-rename notice while unused: the name before it, when.
    pub rename_notice: Option<(String, UnixMs)>,
    /// Its live careers, oldest first.
    pub careers: Vec<CareerRow>,
}

/// The account's live profiles (not erased) with their live careers, oldest first.
pub async fn players_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Vec<PlayerRow>, IdentityError> {
    let parents = sqlx::query(
        "SELECT id, name, mode, xuid, status, created_at FROM player_profile \
         WHERE account_id = $1 AND status <> 'deleted' ORDER BY created_at, id",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    let careers = sqlx::query(
        "SELECT c.player_profile_id, c.game, c.id, c.created_at, c.rename_notice_from, \
           c.rename_notice_at, \
           (SELECT max(s.ended_at) FROM play_session s WHERE s.profile_id = c.id) AS last_played_at, \
           h.revision, r.committed_at, g.profile_id IS NOT NULL AS has_progress, g.level, \
           g.credits, g.cars, g.play_seconds, g.updated_at AS progress_at \
         FROM profile c \
         LEFT JOIN save_head h ON h.profile_id = c.id \
         LEFT JOIN save_revision r ON r.profile_id = c.id AND r.revision = h.revision \
         LEFT JOIN profile_progress g ON g.profile_id = c.id \
         WHERE c.account_id = $1 AND c.status <> 'deleted' ORDER BY c.created_at, c.id",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::with_capacity(parents.len());
    for p in &parents {
        let id: Uuid = p.try_get("id")?;
        let mut mine = Vec::new();
        let mut rename_notice = None;
        for c in &careers {
            if c.try_get::<Uuid, _>("player_profile_id")? != id {
                continue;
            }
            if rename_notice.is_none()
                && let (Some(from), Some(at)) = (
                    c.try_get::<Option<String>, _>("rename_notice_from")?,
                    c.try_get::<Option<OffsetDateTime>, _>("rename_notice_at")?,
                )
            {
                rename_notice = Some((from, ms(at)));
            }
            let revision: Option<i64> = c.try_get("revision")?;
            let committed: Option<OffsetDateTime> = c.try_get("committed_at")?;
            let progress = if c.try_get::<bool, _>("has_progress")? {
                Some(CareerProgressRow {
                    level: c.try_get("level")?,
                    credits: c.try_get("credits")?,
                    cars: c.try_get("cars")?,
                    play_seconds: c.try_get("play_seconds")?,
                    synced_at: ms(c.try_get("progress_at")?),
                })
            } else {
                None
            };
            mine.push(CareerRow {
                game: c.try_get("game")?,
                career_id: c.try_get("id")?,
                created_at: ms(c.try_get("created_at")?),
                last_played_at: c
                    .try_get::<Option<OffsetDateTime>, _>("last_played_at")?
                    .map(ms),
                head: match (revision, committed) {
                    (Some(r), Some(at)) if r > 0 => Some((r, ms(at))),
                    _ => None,
                },
                progress,
            });
        }
        out.push(PlayerRow {
            profile_id: id,
            name: p.try_get("name")?,
            mode: p.try_get("mode")?,
            xuid: p.try_get::<i64, _>("xuid")? as u64,
            status: p.try_get("status")?,
            created_at: ms(p.try_get("created_at")?),
            rename_notice,
            careers: mine,
        });
    }
    Ok(out)
}

/// The account's bound devices.
pub async fn devices_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<Vec<DeviceRow>, IdentityError> {
    let rows = sqlx::query(
        "SELECT id, device_id, label, platform, bound_at, last_seen_at FROM device_binding \
         WHERE account_id = $1 AND unbound_at IS NULL ORDER BY bound_at, id",
    )
    .bind(account_id)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(DeviceRow {
                binding_id: r.try_get("id")?,
                device_id: r.try_get("device_id")?,
                label: r.try_get("label")?,
                platform: r.try_get("platform")?,
                bound_at: ms(r.try_get("bound_at")?),
                last_seen_at: ms(r.try_get("last_seen_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}

/// Binds `device` to the account (or refreshes its live binding), retaining install history.
/// OD-93 removes device quotas; login exclusivity is enforced when issuing launcher families.
/// The caller holds the account lock ([`lock_account_in`], or a new account's insert).
pub async fn bind_device_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    device: &NewDevice,
) -> Result<DeviceRow, IdentityError> {
    let live = sqlx::query(
        "UPDATE device_binding SET label = $3, platform = $4, last_seen_at = clock_timestamp(), \
           fingerprint_hmac = coalesce($5, fingerprint_hmac) \
         WHERE account_id = $1 AND device_id = $2 AND unbound_at IS NULL \
         RETURNING id, device_id, label, platform, bound_at, last_seen_at",
    )
    .bind(account_id)
    .bind(device.device_id)
    .bind(&device.label)
    .bind(&device.platform)
    .bind(device.fingerprint_hmac.map(|h| h.to_vec()))
    .fetch_optional(&mut *conn)
    .await?;
    let row = match live {
        Some(row) => row,
        None => sqlx::query(
            "INSERT INTO device_binding (account_id, device_id, label, platform, fingerprint_hmac) \
                 VALUES ($1, $2, $3, $4, $5) \
                 RETURNING id, device_id, label, platform, bound_at, last_seen_at",
        )
        .bind(account_id)
        .bind(device.device_id)
        .bind(&device.label)
        .bind(&device.platform)
        .bind(device.fingerprint_hmac.map(|h| h.to_vec()))
        .fetch_one(&mut *conn)
        .await?,
    };
    Ok(DeviceRow {
        binding_id: row.try_get("id")?,
        device_id: row.try_get("device_id")?,
        label: row.try_get("label")?,
        platform: row.try_get("platform")?,
        bound_at: ms(row.try_get("bound_at")?),
        last_seen_at: ms(row.try_get("last_seen_at")?),
    })
}

/// Supporter access as `/me` shows it (monetization's read model, ACCOUNTS.md §7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessRow {
    /// `vip` iff a granting status and `access_until` in the future, by the database clock.
    pub vip: bool,
    /// Entitlement status (`none` without a row).
    pub status: String,
    /// End of access.
    pub access_until: Option<UnixMs>,
    /// Plan.
    pub plan: Option<String>,
    /// Renews.
    pub auto_renew: bool,
    /// A renewal charge is failing.
    pub past_due: bool,
}

impl AccessRow {
    /// No entitlement: free (`fi_domain::Access::FREE`).
    pub fn free() -> Self {
        Self {
            vip: false,
            status: "none".into(),
            access_until: None,
            plan: None,
            auto_renew: false,
            past_due: false,
        }
    }
}

/// The account's access: monetization's `entitlement` row for product `vip` when that table exists
/// (MON-M1 creates it), else free. Reads only the columns ADR 0305 §3 names.
pub async fn access_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<AccessRow, IdentityError> {
    let exists: bool = sqlx::query_scalar("SELECT to_regclass('public.entitlement') IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(AccessRow::free());
    }
    let row = sqlx::query(
        "SELECT status, access_until, plan, auto_renew, past_due, \
                (status IN ('active', 'grace') AND access_until > clock_timestamp()) AS vip \
         FROM entitlement WHERE account_id = $1 AND product = 'vip'",
    )
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(r) = row else {
        return Ok(AccessRow::free());
    };
    Ok(AccessRow {
        vip: r.try_get::<Option<bool>, _>("vip")?.unwrap_or(false),
        status: r.try_get("status")?,
        access_until: r
            .try_get::<Option<OffsetDateTime>, _>("access_until")?
            .map(ms),
        plan: r.try_get("plan")?,
        auto_renew: r.try_get::<Option<bool>, _>("auto_renew")?.unwrap_or(false),
        past_due: r.try_get::<Option<bool>, _>("past_due")?.unwrap_or(false),
    })
}

/// Everything `/me` (launcher) and the portal's `Viewer` show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Me {
    /// The account.
    pub account: AccountState,
    /// Live profiles (CR-S98: careers).
    pub profiles: Vec<ProfileRow>,
    /// Live profiles above their careers (CR-S98).
    pub players: Vec<PlayerRow>,
    /// Bound devices.
    pub devices: Vec<DeviceRow>,
    /// Supporter access.
    pub access: AccessRow,
}

/// Reads [`Me`] in one repeatable-read snapshot.
pub async fn me(pool: &PgPool, account_id: Uuid) -> Result<Option<Me>, IdentityError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = $1"
    )))
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let account = state_from_row(&row)?;
    let me = Me {
        profiles: profiles_in(&mut tx, account_id).await?,
        players: players_in(&mut tx, account_id).await?,
        devices: devices_in(&mut tx, account_id).await?,
        access: access_in(&mut tx, account_id).await?,
        account,
    };
    tx.commit().await?;
    Ok(Some(me))
}

/// The outcome of a deletion request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeletionScheduled {
    /// When it was requested.
    pub requested_at: UnixMs,
    /// When the erasure job purges it.
    pub purge_at: UnixMs,
}

/// Requests the account's deletion (ACCOUNTS.md §8): `deletion_pending` with `purgeAt = now + 14
/// days`, its profiles too, every refresh family but `keep_family` revoked. A repeated request returns the pending one.
pub async fn request_deletion_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    keep_family: Uuid,
) -> Result<DeletionScheduled, IdentityError> {
    schedule_deletion_in(conn, account_id, Some(keep_family), "player").await
}

/// An admin's forced erasure (CORE_API.md §5 `erase`): the same 14-day deletion as
/// [`request_deletion_in`], with every refresh family revoked and the audit row's actor
/// `moderator`. A repeated request returns the pending one.
pub async fn force_deletion_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<DeletionScheduled, IdentityError> {
    schedule_deletion_in(conn, account_id, None, "moderator").await
}

async fn schedule_deletion_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    keep_family: Option<Uuid>,
    actor: &str,
) -> Result<DeletionScheduled, IdentityError> {
    let state = lock_account_in(conn, account_id).await?;
    if let Some((requested_at, purge_at)) = state.deletion {
        return Ok(DeletionScheduled {
            requested_at,
            purge_at,
        });
    }
    if state.status != "active" {
        return Err(IdentityError::NotFound);
    }
    let row = sqlx::query(
        "UPDATE account SET status = 'deletion_pending', deletion_requested_at = clock_timestamp(), \
           deletion_due_at = clock_timestamp() + make_interval(days => $2) \
         WHERE id = $1 RETURNING deletion_requested_at, deletion_due_at",
    )
    .bind(account_id)
    .bind(DELETION_GRACE_DAYS as i32)
    .fetch_one(&mut *conn)
    .await?;
    let scheduled = DeletionScheduled {
        requested_at: ms(row.try_get("deletion_requested_at")?),
        purge_at: ms(row.try_get("deletion_due_at")?),
    };
    sqlx::query(
        "UPDATE profile SET status = 'deletion_pending', deletion_due_at = $2 \
         WHERE account_id = $1 AND status = 'active'",
    )
    .bind(account_id)
    .bind(crate::identity::ts(scheduled.purge_at))
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE player_profile SET status = 'deletion_pending' WHERE account_id = $1 AND status = 'active'",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE refresh_token SET revoked_at = clock_timestamp(), revoke_reason = 'deletion_requested' \
         WHERE account_id = $1 AND ($2::uuid IS NULL OR family_id <> $2) AND revoked_at IS NULL",
    )
    .bind(account_id)
    .bind(keep_family)
    .execute(&mut *conn)
    .await?;
    audit_as_in(conn, account_id, "account.deletion_requested", actor).await?;
    Ok(scheduled)
}

/// Cancels a pending deletion: the account and its pending profiles are active again.
pub async fn cancel_deletion_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(), IdentityError> {
    let state = lock_account_in(conn, account_id).await?;
    if state.status != "deletion_pending" {
        return Err(IdentityError::NotPendingDeletion);
    }
    sqlx::query(
        "UPDATE account SET status = 'active', deletion_requested_at = NULL, deletion_due_at = NULL WHERE id = $1",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE profile SET status = 'active', deletion_due_at = NULL \
         WHERE account_id = $1 AND status = 'deletion_pending'",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE player_profile SET status = 'active' WHERE account_id = $1 AND status = 'deletion_pending'",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    audit_in(conn, account_id, "account.deletion_cancelled").await?;
    Ok(())
}

/// One `account_audit` row (no IP, no e-mail: ARCHITECTURE.md §8).
pub async fn audit_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    kind: &str,
) -> Result<(), IdentityError> {
    audit_as_in(conn, account_id, kind, "player").await
}

/// [`audit_in`] with another actor (`player`, `system`, `moderator`, `import`).
pub async fn audit_as_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    kind: &str,
    actor: &str,
) -> Result<(), IdentityError> {
    sqlx::query("INSERT INTO account_audit (account_id, kind, actor) VALUES ($1, $2, $3)")
        .bind(account_id)
        .bind(kind)
        .bind(actor)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// A player's public card (CORE_API.md §4): a live, listed, not hidden profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardRow {
    /// Profile id.
    pub profile_id: Uuid,
    /// Account (never on the wire of a public card).
    pub account_id: Uuid,
    /// XUID.
    pub xuid: u64,
    /// Game.
    pub game: String,
    /// `Name#12345`.
    pub gamertag: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// Level from the progress projection.
    pub level: Option<i32>,
    /// The owner is a Supporter now.
    pub vip: bool,
    /// The profile has an active play session.
    pub online: bool,
    /// Profile created.
    pub since: UnixMs,
}

const CARD_SQL: &str = "SELECT p.id, p.account_id, p.xuid, p.game, p.name, p.mode, p.created_at, pp.level, \
       COALESCE(e.status IN ('active', 'grace') AND e.access_until > clock_timestamp(), false) AS vip, \
       EXISTS (SELECT 1 FROM play_session s WHERE s.profile_id = p.id AND s.state = 'active' AND s.expires_at > clock_timestamp() \
         AND NOT s.dedicated) AS online \
     FROM profile p LEFT JOIN profile_progress pp ON pp.profile_id = p.id \
     LEFT JOIN entitlement e ON e.account_id = p.account_id AND e.product = 'vip' \
     WHERE p.status = 'active' AND p.social_listed AND p.social_hidden_at IS NULL";

fn card_from_row(r: &PgRow) -> Result<CardRow, sqlx::Error> {
    Ok(CardRow {
        profile_id: r.try_get("id")?,
        account_id: r.try_get("account_id")?,
        xuid: r.try_get::<i64, _>("xuid")? as u64,
        game: r.try_get("game")?,
        gamertag: r.try_get("name")?,
        mode: r.try_get("mode")?,
        level: r.try_get("level")?,
        vip: r.try_get("vip")?,
        online: r.try_get("online")?,
        since: ms(r.try_get("created_at")?),
    })
}

/// The player directory of `game`: listed profiles whose name starts with `prefix`
/// (case-insensitive, ASCII letters and digits only), by name then XUID, after the cursor
/// `(lower(name), xuid)`.
pub async fn directory_in(
    conn: &mut PgConnection,
    game: &str,
    prefix: Option<&str>,
    after: Option<(&str, u64)>,
    limit: i64,
) -> Result<Vec<CardRow>, IdentityError> {
    let prefix = prefix.map(|p| {
        p.chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
    });
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{CARD_SQL} AND p.game = $1 AND ($2::text IS NULL OR lower(p.name) LIKE lower($2) || '%') \
           AND ($3::text IS NULL OR (lower(p.name), p.xuid) > ($3, $4)) \
         ORDER BY lower(p.name), p.xuid LIMIT $5"
    )))
    .bind(game)
    .bind(prefix)
    .bind(after.map(|(name, _)| name.to_owned()))
    .bind(after.map(|(_, xuid)| xuid as i64))
    .bind(limit)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.iter().map(card_from_row).collect::<Result<_, _>>()?)
}

/// One player's card; `None` when the profile is unknown, deleted, unlisted or hidden.
pub async fn card_in(conn: &mut PgConnection, xuid: u64) -> Result<Option<CardRow>, IdentityError> {
    // CR-S98: a XUID is every career's of one profile; the card is its oldest listed career's
    // (FH2's for every profile created before CR-S98).
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "{CARD_SQL} AND p.xuid = $1 ORDER BY p.created_at, p.id LIMIT 1"
    )))
    .bind(xuid as i64)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(card_from_row).transpose()?)
}

/// A profile's progress projection (`profile_progress`, kept by the writers' transactions and `consolidate-nightly`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressRow {
    /// Level.
    pub level: Option<i32>,
    /// Experience.
    pub xp: Option<i64>,
    /// Credits.
    pub credits: Option<i64>,
    /// Cars owned.
    pub cars: Option<i32>,
    /// Races finished.
    pub races: i32,
    /// Seconds played.
    pub play_seconds: i64,
}

/// The progress of `profile_id`, when the projection has a row.
pub async fn progress_in(
    conn: &mut PgConnection,
    profile_id: Uuid,
) -> Result<Option<ProgressRow>, IdentityError> {
    let row = sqlx::query(
        "SELECT level, xp, credits, cars, races, play_seconds FROM profile_progress WHERE profile_id = $1",
    )
    .bind(profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(ProgressRow {
            level: r.try_get("level")?,
            xp: r.try_get("xp")?,
            credits: r.try_get("credits")?,
            cars: r.try_get("cars")?,
            races: r.try_get("races")?,
            play_seconds: r.try_get("play_seconds")?,
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// What the Discord presence shows of one profile (CR-S72, OD-136, ACCOUNTS.md §5 "Presence").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceRow {
    /// The profile's club of its mode: `(tag, name)`.
    pub club: Option<(String, String)>,
    /// The last accepted `UpdateAll` of its mode (`integrity_baseline`): level, credits, when. The
    /// level is the driver level the baseline's XP reaches (CR-S81: rows written before the fix
    /// hold the game's in-band step).
    pub synced: Option<(i64, i64, UnixMs)>,
}

/// The presence of the account's own live profile `xuid`, or `None` when the account holds no
/// such profile.
pub async fn presence_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    xuid: u64,
    game: &str,
) -> Result<Option<PresenceRow>, IdentityError> {
    let Ok(xuid) = i64::try_from(xuid) else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT c.tag, c.name, b.xp, b.credits, b.synced_at \
           FROM profile p \
           LEFT JOIN game_club_member m ON m.game = p.game AND m.profile_id = p.id \
           LEFT JOIN game_club c ON c.game = m.game AND c.id = m.club_id AND c.mode = p.mode \
           LEFT JOIN integrity_baseline b \
                  ON b.game = p.game AND b.profile_id = p.id AND b.mode = p.mode \
                 AND b.synced_at IS NOT NULL \
          WHERE p.account_id = $1 AND p.xuid = $2 AND p.game = $3 AND p.status <> 'deleted'",
    )
    .bind(account_id)
    .bind(xuid)
    .bind(game)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        let tag: Option<String> = r.try_get("tag")?;
        let name: Option<String> = r.try_get("name")?;
        let level = r
            .try_get::<Option<i64>, _>("xp")?
            .map(fi_gamedb::gates::driver_level);
        let credits: Option<i64> = r.try_get("credits")?;
        let synced: Option<OffsetDateTime> = r.try_get("synced_at")?;
        Ok(PresenceRow {
            club: tag.zip(name),
            synced: match (level, credits, synced) {
                (Some(l), Some(c), Some(at)) => Some((l, c, ms(at))),
                _ => None,
            },
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// One profile of an account that holds more than one live profile of a mode (CR-S98, OD-207;
/// CAREERS.md §2), as `fi-cli profiles duplicates` reports it: ids and ranking values only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicateRow {
    /// The account.
    pub account_id: Uuid,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The profile.
    pub profile_id: Uuid,
    /// Its rank in the account's mode, 1 the profile kept (study §8.1).
    pub rank: i64,
    /// Highest `profile_progress.xp` of its live careers.
    pub xp: Option<i64>,
    /// Highest `profile_progress.play_seconds` of its live careers.
    pub play_seconds: Option<i64>,
    /// Newest head revision's commit time of its live careers.
    pub saved_at: Option<UnixMs>,
    /// Created.
    pub created_at: UnixMs,
}

/// Every live profile of each (account, mode) holding more than one, ranked as the
/// MULTI_GAME_PROFILE_STUDY.md §8.1 cleanup keeps them: highest XP, then play time, then the newest
/// save head, then the oldest; a profile without a progress row ranks after every one with. Reads
/// only, in a `READ ONLY` transaction; accounts ordered by id, ranks ascending.
pub async fn duplicate_profiles(pool: &PgPool) -> Result<Vec<DuplicateRow>, IdentityError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query(
        "WITH ranked AS ( \
           SELECT p.account_id, p.mode, p.id, p.created_at, s.xp, s.play_seconds, s.saved_at, \
                  coalesce(s.has_progress, false) AS has_progress, \
                  count(*) OVER (PARTITION BY p.account_id, p.mode) AS n \
             FROM player_profile p \
             CROSS JOIN LATERAL ( \
               SELECT max(g.xp) AS xp, max(g.play_seconds) AS play_seconds, \
                      max(r.committed_at) AS saved_at, bool_or(g.profile_id IS NOT NULL) AS has_progress \
                 FROM profile c \
                 LEFT JOIN profile_progress g ON g.profile_id = c.id \
                 LEFT JOIN save_head h ON h.profile_id = c.id \
                 LEFT JOIN save_revision r ON r.profile_id = c.id AND r.revision = h.revision \
                WHERE c.player_profile_id = p.id AND c.status <> 'deleted') s \
            WHERE p.status <> 'deleted') \
         SELECT account_id, mode, id, created_at, xp, play_seconds, saved_at, \
                row_number() OVER (PARTITION BY account_id, mode \
                  ORDER BY has_progress DESC, xp DESC NULLS LAST, play_seconds DESC NULLS LAST, \
                           saved_at DESC NULLS LAST, created_at, id) AS rank \
           FROM ranked WHERE n > 1 ORDER BY account_id, mode, rank",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    rows.iter()
        .map(|r| {
            Ok(DuplicateRow {
                account_id: r.try_get("account_id")?,
                mode: r.try_get("mode")?,
                profile_id: r.try_get("id")?,
                rank: r.try_get("rank")?,
                xp: r.try_get("xp")?,
                play_seconds: r.try_get("play_seconds")?,
                saved_at: r.try_get::<Option<OffsetDateTime>, _>("saved_at")?.map(ms),
                created_at: ms(r.try_get("created_at")?),
            })
        })
        .collect::<Result<_, sqlx::Error>>()
        .map_err(Into::into)
}
