//! The Identity Platform exchange's storage (ACCOUNTS.md §4.1, §5; migration `..._identity.sql`):
//! identity tickets and the legacy site-key
//! link, plus the error type the identity repositories share ([`IdentityError`]).
//!
//! Every function takes the caller's connection (`*_in`, usually inside its business transaction),
//! reads the **database clock** (`clock_timestamp()`), and stores secrets only as the SHA-256 the
//! caller computed (fi-auth mints them; this crate never sees a secret). A credential is spent by
//! an `UPDATE ... WHERE <unused> AND expires_at > clock_timestamp() RETURNING`, so of two concurrent
//! redemptions exactly one gets the row; if the caller's transaction rolls back (a later check
//! failed), the credential is unspent again, so a typing mistake never burns a ticket.

use crate::limits::{self, Idempotency};
use fi_domain::UnixMs;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Identity tickets live 15 minutes.
pub const TICKET_TTL_SECS: i64 = 900;
/// Why an identity-plane write could not happen (fi-api maps each to its `ErrorCode`).
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// Any other database failure (`SERVICE.UNAVAILABLE`).
    #[error("database: {0}")]
    Db(sqlx::Error),
    /// A lock or statement timeout: retry later (`SERVICE.UNAVAILABLE` with `Retry-After`).
    #[error("database busy")]
    Busy,
    /// The name is taken in the game or reserved (`ACCOUNTS.NAME_UNAVAILABLE`, OD-120).
    #[error("name unavailable")]
    NameUnavailable,
    /// A live profile of this mode already (`ACCOUNTS.PROFILE_LIMIT`, CR-S98).
    #[error("profile limit reached")]
    ProfileLimit,
    /// The account is banned (`ACCOUNTS.BANNED {review?}`; CR-S98: a career registered through
    /// `POST /profiles`, CAREERS.md §6.6, as `POST /players/{xuid}/careers` refuses it).
    #[error("banned")]
    Banned {
        /// `account.ban_review` while the ban is in force.
        review: Option<String>,
    },
    /// The identity already has an account, or the account another identity
    /// (`ACCOUNTS.ALREADY_LINKED`).
    #[error("already linked")]
    AlreadyLinked,
    /// `SHARING.TOO_MANY_DEVICES`.
    #[error("too many devices")]
    TooManyDevices,
    /// `SHARING.TOO_MANY_RECENT_DEVICES`.
    #[error("too many recent devices")]
    TooManyRecentDevices,
    /// The account or profile does not exist (or is erased).
    #[error("not found")]
    NotFound,
    /// `ACCOUNTS.NOT_PENDING_DELETION`.
    #[error("account is not pending deletion")]
    NotPendingDeletion,
    /// The same `Idempotency-Key` with another request (`REQUEST.IDEMPOTENCY_KEY_REUSED`).
    #[error("idempotency key reused")]
    IdempotencyKeyReused,
    /// An event did not validate (a bug: nothing is written).
    #[error("event: {0}")]
    Event(String),
}

impl From<sqlx::Error> for IdentityError {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            // 55P03 lock_not_available, 57014 query_canceled, 25P04 transaction_timeout,
            // 40001 serialization_failure, 40P01 deadlock_detected.
            sqlx::Error::Database(db)
                if matches!(
                    db.code().as_deref(),
                    Some("55P03" | "57014" | "25P04" | "40001" | "40P01")
                ) =>
            {
                IdentityError::Busy
            }
            _ => IdentityError::Db(e),
        }
    }
}

/// Whether `e` is a unique violation of `constraint`.
pub(crate) fn is_unique_violation(e: &sqlx::Error, constraint: &str) -> bool {
    matches!(e, sqlx::Error::Database(db)
        if db.code().as_deref() == Some("23505") && db.constraint() == Some(constraint))
}

pub(crate) fn ms(t: OffsetDateTime) -> UnixMs {
    UnixMs::from_millis((t.unix_timestamp_nanos().div_euclid(1_000_000)) as i64)
}

pub(crate) fn ts(t: UnixMs) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(t.millis()) * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The database clock.
pub async fn now_in(conn: &mut PgConnection) -> Result<UnixMs, IdentityError> {
    let now: OffsetDateTime = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *conn)
        .await?;
    Ok(ms(now))
}

/// Which client an identity ticket was minted for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// `/launcher/v1`.
    Launcher,
    /// `/portal/v1`.
    Portal,
}

impl Client {
    /// The column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Client::Launcher => "launcher",
            Client::Portal => "portal",
        }
    }
}

/// What a verified ID token proved, as stored with identity tickets. Never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct IdentityClaim {
    /// Identity Platform uid.
    pub idp_uid: String,
    /// Sign-in provider of that exchange.
    pub provider_id: String,
    /// E-mail in lower case.
    pub email_lower: Option<String>,
    /// Whether Identity Platform verified it.
    pub email_verified: bool,
    /// When the person actually signed in (the ID token's `auth_time`).
    pub auth_time: UnixMs,
}

impl std::fmt::Debug for IdentityClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdentityClaim")
            .field("provider_id", &self.provider_id)
            .field("auth_time", &self.auth_time)
            .finish_non_exhaustive()
    }
}

fn claim_from_row(row: &PgRow) -> Result<IdentityClaim, sqlx::Error> {
    Ok(IdentityClaim {
        idp_uid: row.try_get("idp_uid")?,
        provider_id: row.try_get("provider_id")?,
        email_lower: row.try_get("email_lower")?,
        email_verified: row.try_get("email_verified")?,
        auth_time: ms(row.try_get("auth_time")?),
    })
}

/// A minted credential: its id (for idempotency records) and expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Minted {
    /// Row id.
    pub id: Uuid,
    /// Expiry (database clock).
    pub expires_at: UnixMs,
}

// --- identity tickets ------------------------------------------------------------------------

/// Stores a ticket (`ticket_sha256`) for `claim`, valid 15 minutes, for `client`.
pub async fn mint_ticket_in(
    conn: &mut PgConnection,
    ticket_sha256: &[u8; 32],
    claim: &IdentityClaim,
    client: Client,
) -> Result<Minted, IdentityError> {
    let row = sqlx::query(
        "INSERT INTO identity_ticket (ticket_sha256, idp_uid, provider_id, email_lower, email_verified, client, auth_time, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, clock_timestamp() + make_interval(secs => $8)) RETURNING id, expires_at",
    )
    .bind(ticket_sha256.as_slice())
    .bind(&claim.idp_uid)
    .bind(&claim.provider_id)
    .bind(&claim.email_lower)
    .bind(claim.email_verified)
    .bind(client.as_str())
    .bind(ts(claim.auth_time))
    .bind(TICKET_TTL_SECS as f64)
    .fetch_one(&mut *conn)
    .await?;
    Ok(Minted {
        id: row.try_get("id")?,
        expires_at: ms(row.try_get("expires_at")?),
    })
}

/// A redeemed ticket.
#[derive(Clone, Debug)]
pub struct Ticket {
    /// Row id.
    pub id: Uuid,
    /// The identity it carries.
    pub claim: IdentityClaim,
}

/// Spends the unused, unexpired ticket `ticket_sha256` of `client`; `None` if there is none.
pub async fn consume_ticket_in(
    conn: &mut PgConnection,
    ticket_sha256: &[u8; 32],
    client: Client,
) -> Result<Option<Ticket>, IdentityError> {
    let row = sqlx::query(
        "UPDATE identity_ticket SET used_at = clock_timestamp() \
         WHERE ticket_sha256 = $1 AND client = $2 AND used_at IS NULL AND expires_at > clock_timestamp() \
         RETURNING id, idp_uid, provider_id, email_lower, email_verified, auth_time",
    )
    .bind(ticket_sha256.as_slice())
    .bind(client.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(Ticket {
            id: r.try_get("id")?,
            claim: claim_from_row(&r)?,
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// Replay of an exchange whose `needs_account` answer was lost (CR-L4): mints `new_sha256` with the
/// claims of ticket `ticket_id` and spends that one, provided it was never redeemed. `None` when it
/// was (the client did receive it) or it expired.
pub async fn reissue_ticket_in(
    conn: &mut PgConnection,
    ticket_id: Uuid,
    new_sha256: &[u8; 32],
) -> Result<Option<Minted>, IdentityError> {
    let row = sqlx::query(
        "WITH spent AS ( \
           UPDATE identity_ticket SET used_at = clock_timestamp() \
           WHERE id = $1 AND used_at IS NULL AND expires_at > clock_timestamp() \
           RETURNING idp_uid, provider_id, email_lower, email_verified, client, auth_time) \
         INSERT INTO identity_ticket (ticket_sha256, idp_uid, provider_id, email_lower, email_verified, client, auth_time, expires_at) \
         SELECT $2, idp_uid, provider_id, email_lower, email_verified, client, auth_time, clock_timestamp() + make_interval(secs => $3) FROM spent \
         RETURNING id, expires_at",
    )
    .bind(ticket_id)
    .bind(new_sha256.as_slice())
    .bind(TICKET_TTL_SECS as f64)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(Minted {
            id: r.try_get("id")?,
            expires_at: ms(r.try_get("expires_at")?),
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// [`reissue_ticket_in`] for the request `(scope, idem)`, whose record is then re-pointed at the
/// new ticket in the same transaction: the next replay spends this one, so at most one unspent
/// ticket lives per `(scope, key)`. Take [`limits::lock_key_in`] first.
pub async fn replay_ticket_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
    ticket_id: Uuid,
    new_sha256: &[u8; 32],
) -> Result<Option<Minted>, IdentityError> {
    let Some(minted) = reissue_ticket_in(conn, ticket_id, new_sha256).await? else {
        return Ok(None);
    };
    limits::repoint_in(conn, scope, idem, minted.id).await?;
    Ok(Some(minted))
}

/// The uid a ticket of `client` was minted for, spent or not, while unexpired: the idempotency
/// scope of a portal request that redeems it (a replay finds its record through it).
pub async fn ticket_uid_in(
    conn: &mut PgConnection,
    ticket_sha256: &[u8; 32],
    client: Client,
) -> Result<Option<String>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT idp_uid FROM identity_ticket WHERE ticket_sha256 = $1 AND client = $2 AND expires_at > clock_timestamp()",
    )
    .bind(ticket_sha256.as_slice())
    .bind(client.as_str())
    .fetch_optional(&mut *conn)
    .await?)
}

// --- legacy link (ACCOUNTS.md §4.1, §10) ---------------------------------------------------------

/// An imported Beta account found by gamertag, with its unspent site-key hash (row locked).
#[derive(Clone)]
pub struct LegacyAccount {
    /// The account.
    pub account_id: Uuid,
    /// `HMAC(pepper, canonical key)` (keys issued by this stack; none since OD-20).
    pub key_hmac: Option<[u8; 32]>,
    /// `HMAC(pepper, hex(SHA-1(canonical key)))` (imported Beta keys).
    pub legacy_wrapped: Option<[u8; 32]>,
    /// The pepper version of the hash.
    pub pepper_version: u16,
    /// Whether another identity is mapped to the account already.
    pub linked: bool,
    /// `banned_at` is set.
    pub banned: bool,
    /// The ban's review (`account.ban_review`, CR-S52) while banned.
    pub ban_review: Option<String>,
}

impl std::fmt::Debug for LegacyAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LegacyAccount")
            .field("account_id", &self.account_id)
            .field("linked", &self.linked)
            .finish_non_exhaustive()
    }
}

fn hash32(v: Option<Vec<u8>>) -> Option<[u8; 32]> {
    v.and_then(|b| b.try_into().ok())
}

/// The active account owning the legacy gamertag `name#discriminator` of `game` (OD-120: the name
/// that went with the number, whatever the profile is called now) with an unspent site key
/// (locked `FOR UPDATE`), or `None`. Unknown gamertags, accounts without a key and spent keys look
/// the same to the caller (`AUTH.INVALID_CREDENTIALS`).
pub async fn find_legacy_account_in(
    conn: &mut PgConnection,
    game: &str,
    name: &str,
    discriminator: u32,
) -> Result<Option<LegacyAccount>, IdentityError> {
    let row = sqlx::query(
        "SELECT a.id, (a.banned_at IS NOT NULL AND (a.banned_until IS NULL OR a.banned_until > clock_timestamp())) AS banned, CASE WHEN a.banned_at IS NOT NULL AND (a.banned_until IS NULL OR a.banned_until > clock_timestamp()) THEN a.ban_review END AS ban_review, k.key_hmac, k.legacy_wrapped, k.pepper_version, \
                EXISTS (SELECT 1 FROM account_identity i WHERE i.account_id = a.id) AS linked \
         FROM profile p JOIN account a ON a.id = p.account_id JOIN account_site_key k ON k.account_id = a.id \
         WHERE p.game = $1 AND lower(coalesce(p.legacy_name, p.name)) = lower($2) AND p.discriminator = $3 \
           AND p.status <> 'deleted' \
           AND a.status = 'active' AND k.spent_at IS NULL \
         FOR UPDATE OF k",
    )
    .bind(game)
    .bind(name)
    .bind(discriminator as i32)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(LegacyAccount {
            account_id: r.try_get("id")?,
            key_hmac: hash32(r.try_get("key_hmac")?),
            legacy_wrapped: hash32(r.try_get("legacy_wrapped")?),
            pepper_version: r.try_get::<i16, _>("pepper_version")? as u16,
            linked: r.try_get("linked")?,
            banned: r.try_get("banned")?,
            ban_review: r.try_get("ban_review")?,
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// Spends every legacy credential of `account_id`: the site-key hash is overwritten with random
/// bytes and marked spent (the runtime role has no DELETE; fi_maintenance prunes the row), and the
/// imported Beta device tokens are marked exchanged. Neither signs in or links again.
pub async fn spend_legacy_credentials_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE account_site_key SET \
           key_hmac = CASE WHEN key_hmac IS NULL THEN NULL ELSE uuid_send(gen_random_uuid()) || uuid_send(gen_random_uuid()) END, \
           legacy_wrapped = CASE WHEN legacy_wrapped IS NULL THEN NULL ELSE uuid_send(gen_random_uuid()) || uuid_send(gen_random_uuid()) END, \
           spent_at = clock_timestamp() \
         WHERE account_id = $1 AND spent_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE legacy_device_token SET exchanged_at = clock_timestamp() \
         WHERE exchanged_at IS NULL AND profile_id IN (SELECT id FROM profile WHERE account_id = $1)",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
