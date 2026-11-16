//! A `local` instance's e-mail and password credentials (ACCOUNTS.md §4.1 "Two identity
//! backends", OD-122 M2.3; migration `..._local_credential.sql`). The caller normalizes the e-mail
//! and hashes the password (fi-auth's Argon2id): this crate never sees a password, and nothing here
//! logs an address or a hash. Every function takes the caller's connection.

use crate::identity::{IdentityError, is_unique_violation, ms};
use fi_domain::UnixMs;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// A stored credential. `Debug` prints neither the e-mail nor the hash.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// The credential id: the `sub` of the instance's ID tokens.
    pub id: Uuid,
    /// The normalized e-mail.
    pub email_lower: String,
    /// The Argon2id PHC string, an imported Firebase hash (`$firebase-scrypt$…`, rehashed at the
    /// first successful sign-in), or `None`: "password not set" (only the set-password mail
    /// gives it one).
    pub password_hash: Option<String>,
    /// When the address was confirmed (`None`: unconfirmed).
    pub email_verified_at: Option<UnixMs>,
    /// Wrong confirmation codes over its life (0 again once confirmed).
    pub code_failures: i32,
    /// When it was created (database clock).
    pub created_at: UnixMs,
    /// When its password last changed (database clock; `created_at` until then).
    pub updated_at: UnixMs,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// What [`create_in`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Created {
    /// A new credential.
    New(Credential),
    /// The address already has one (`ACCOUNTS.EMAIL_TAKEN`).
    EmailTaken,
}

const COLUMNS: &str =
    "id, email_lower, password_hash, email_verified_at, code_failures, created_at, updated_at";

fn from_row(row: &PgRow) -> Result<Credential, sqlx::Error> {
    Ok(Credential {
        id: row.try_get("id")?,
        email_lower: row.try_get("email_lower")?,
        password_hash: row.try_get("password_hash")?,
        email_verified_at: row
            .try_get::<Option<time::OffsetDateTime>, _>("email_verified_at")?
            .map(ms),
        code_failures: row.try_get("code_failures")?,
        created_at: ms(row.try_get("created_at")?),
        updated_at: ms(row.try_get("updated_at")?),
    })
}

/// Creates the credential of `email_lower` (a savepoint, so a taken address leaves the caller's
/// transaction usable), confirmed at once when `confirmed` (an instance that delivers no mail).
pub async fn create_in(
    conn: &mut PgConnection,
    email_lower: &str,
    password_hash: &str,
    confirmed: bool,
) -> Result<Created, IdentityError> {
    insert_in(conn, email_lower, Some(password_hash), confirmed).await
}

/// Inserts one credential (`password_hash` `None`: password not set).
async fn insert_in(
    conn: &mut PgConnection,
    email_lower: &str,
    password_hash: Option<&str>,
    confirmed: bool,
) -> Result<Created, IdentityError> {
    let mut sp = sqlx::Connection::begin(&mut *conn).await?;
    let inserted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO credential (email_lower, password_hash, email_verified_at) \
         VALUES ($1, $2, CASE WHEN $3 THEN clock_timestamp() END) RETURNING {COLUMNS}"
    )))
    .bind(email_lower)
    .bind(password_hash)
    .bind(confirmed)
    .fetch_one(&mut *sp)
    .await;
    match inserted {
        Ok(row) => {
            let credential = from_row(&row)?;
            sp.commit().await?;
            Ok(Created::New(credential))
        }
        Err(e) if is_unique_violation(&e, "credential_email_key") => {
            sp.rollback().await?;
            Ok(Created::EmailTaken)
        }
        Err(e) => Err(e.into()),
    }
}

/// The credential of `email_lower`, if any.
pub async fn find_by_email_in(
    conn: &mut PgConnection,
    email_lower: &str,
) -> Result<Option<Credential>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM credential WHERE email_lower = $1"
    )))
    .bind(email_lower)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(from_row).transpose()?)
}

/// The credential `id`, if any.
pub async fn find_by_id_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<Credential>, IdentityError> {
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM credential WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.as_ref().map(from_row).transpose()?)
}

/// Replaces the password hash of credential `id`; `false` when it does not exist.
pub async fn set_password_hash_in(
    conn: &mut PgConnection,
    id: Uuid,
    password_hash: &str,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE credential SET password_hash = $2, updated_at = greatest(clock_timestamp(), created_at) \
         WHERE id = $1",
    )
    .bind(id)
    .bind(password_hash)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The account mapped to identity uid `idp_uid` (a credential id on a `local` instance), if any.
pub async fn account_of_in(
    conn: &mut PgConnection,
    idp_uid: &str,
) -> Result<Option<Uuid>, IdentityError> {
    Ok(
        sqlx::query_scalar("SELECT account_id FROM account_identity WHERE idp_uid = $1")
            .bind(idp_uid)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// Replaces an imported hash with its Argon2id rehash after a successful sign-in, only while the
/// stored hash is still `previous` (a concurrent change wins). Not a password change: `updated_at`
/// stays, so the ID tokens of this sign-in stay valid. `true` when it replaced it.
pub async fn rehash_in(
    conn: &mut PgConnection,
    id: Uuid,
    previous: &str,
    password_hash: &str,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE credential SET password_hash = $3 WHERE id = $1 AND password_hash = $2",
    )
    .bind(id)
    .bind(previous)
    .bind(password_hash)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// What a mail token is for (`credential_mail_token.purpose`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailPurpose {
    /// Confirms the address (24 hours).
    Confirm,
    /// Sets a new password and confirms the address (1 hour).
    Reset,
}

impl MailPurpose {
    /// The stored value.
    pub const fn as_str(self) -> &'static str {
        match self {
            MailPurpose::Confirm => "confirm",
            MailPurpose::Reset => "reset",
        }
    }

    /// Its life in seconds.
    pub const fn ttl_secs(self) -> i64 {
        match self {
            MailPurpose::Confirm => 86_400,
            MailPurpose::Reset => 3_600,
        }
    }
}

/// Stores a mail token's SHA-256 for credential `id` (the token itself is never stored).
pub async fn insert_mail_token_in(
    conn: &mut PgConnection,
    id: Uuid,
    purpose: MailPurpose,
    token_sha256: &[u8; 32],
) -> Result<(), IdentityError> {
    sqlx::query(
        // One instant for both columns: two clock_timestamp() calls differ by microseconds and the
        // CHECK allows exactly one day.
        "INSERT INTO credential_mail_token (token_sha256, credential_id, purpose, created_at, expires_at) \
         SELECT $1, $2, $3, t.now, t.now + make_interval(secs => $4) \
         FROM (SELECT clock_timestamp() AS now) t",
    )
    .bind(token_sha256.as_slice())
    .bind(id)
    .bind(purpose.as_str())
    .bind(purpose.ttl_secs() as f64)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Wrong codes that spend a confirmation code (and its link).
pub const CODE_ATTEMPTS: i32 = 5;

/// Stores a confirmation token's SHA-256 with the MAC of its 6-digit code (CR-S53 follow-up), in
/// one row: the code shares the link's 24 hours and single use.
pub async fn insert_confirm_token_in(
    conn: &mut PgConnection,
    id: Uuid,
    token_sha256: &[u8; 32],
    code_mac: &[u8; 32],
) -> Result<(), IdentityError> {
    sqlx::query(
        "INSERT INTO credential_mail_token (token_sha256, credential_id, purpose, created_at, expires_at, code_mac) \
         SELECT $1, $2, 'confirm', t.now, t.now + make_interval(secs => $3), $4 \
         FROM (SELECT clock_timestamp() AS now) t",
    )
    .bind(token_sha256.as_slice())
    .bind(id)
    .bind(MailPurpose::Confirm.ttl_secs() as f64)
    .bind(code_mac.as_slice())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The open confirmation code of credential `id` (the newest open `confirm` row with a code),
/// locked for this transaction: `(token_sha256, code_mac)`.
pub async fn open_code_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<([u8; 32], Vec<u8>)>, IdentityError> {
    let row: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT token_sha256, code_mac FROM credential_mail_token \
         WHERE credential_id = $1 AND purpose = 'confirm' AND used_at IS NULL \
           AND expires_at > clock_timestamp() AND code_mac IS NOT NULL \
         ORDER BY created_at DESC LIMIT 1 FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.and_then(|(t, m)| <[u8; 32]>::try_from(t.as_slice()).ok().map(|t| (t, m))))
}

/// Wrong codes over a credential's life after which the code route refuses (M-C1).
pub const CODE_FAILURES_LIFETIME: i32 = 30;

/// Locks credential `id` for the caller's transaction (serializes a resend, a takeover and code
/// submissions on one credential); `false` when it does not exist.
pub async fn lock_in(conn: &mut PgConnection, id: Uuid) -> Result<bool, IdentityError> {
    Ok(
        sqlx::query_scalar::<_, i32>("SELECT 1 FROM credential WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
            .is_some(),
    )
}

/// Counts one wrong code on credential `id` over its life; returns the count.
pub async fn code_failure_in(conn: &mut PgConnection, id: Uuid) -> Result<i32, IdentityError> {
    Ok(sqlx::query_scalar(
        "UPDATE credential SET code_failures = code_failures + 1 WHERE id = $1 RETURNING code_failures",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?)
}

/// Counts one wrong code on the row `token_sha256`; the [`CODE_ATTEMPTS`]th spends it. Returns
/// the attempts so far.
pub async fn wrong_code_in(
    conn: &mut PgConnection,
    token_sha256: &[u8; 32],
) -> Result<i32, IdentityError> {
    Ok(sqlx::query_scalar(
        "UPDATE credential_mail_token SET code_attempts = least(code_attempts + 1, $2), \
         used_at = CASE WHEN code_attempts + 1 >= $2 THEN clock_timestamp() ELSE used_at END \
         WHERE token_sha256 = $1 RETURNING code_attempts",
    )
    .bind(token_sha256.as_slice())
    .bind(CODE_ATTEMPTS)
    .fetch_one(&mut *conn)
    .await?)
}

/// The credential of an open (unused, unexpired) mail token of `purpose`, without spending it.
pub async fn open_mail_token_in(
    conn: &mut PgConnection,
    purpose: MailPurpose,
    token_sha256: &[u8; 32],
) -> Result<Option<Uuid>, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT credential_id FROM credential_mail_token \
         WHERE token_sha256 = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > clock_timestamp()",
    )
    .bind(token_sha256.as_slice())
    .bind(purpose.as_str())
    .fetch_optional(&mut *conn)
    .await?)
}

/// Spends an open mail token of `purpose` (once: a second call, an expired or unknown token answer
/// `None`) and returns its credential.
pub async fn spend_mail_token_in(
    conn: &mut PgConnection,
    purpose: MailPurpose,
    token_sha256: &[u8; 32],
) -> Result<Option<Uuid>, IdentityError> {
    Ok(sqlx::query_scalar(
        "UPDATE credential_mail_token SET used_at = clock_timestamp() \
         WHERE token_sha256 = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > clock_timestamp() \
         RETURNING credential_id",
    )
    .bind(token_sha256.as_slice())
    .bind(purpose.as_str())
    .fetch_optional(&mut *conn)
    .await?)
}

/// Spends every open mail token of credential `id` (after a reset: older links stop working).
pub async fn spend_all_mail_tokens_in(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "UPDATE credential_mail_token SET used_at = clock_timestamp() \
         WHERE credential_id = $1 AND used_at IS NULL",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// Spends the open mail tokens of `purpose` of credential `id` (a new reset link replaces the
/// older ones).
pub async fn spend_mail_tokens_of_in(
    conn: &mut PgConnection,
    id: Uuid,
    purpose: MailPurpose,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "UPDATE credential_mail_token SET used_at = clock_timestamp() \
         WHERE credential_id = $1 AND purpose = $2 AND used_at IS NULL",
    )
    .bind(id)
    .bind(purpose.as_str())
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// Takes over an **unconfirmed** credential that no FI account names (security review M1): its
/// holder proved nothing, so a new sign-up of the address replaces the password (moving
/// `updated_at`: the holder's ID tokens die) and spends its open links. `false` when the
/// credential is confirmed, mapped to an account, or gone (the caller answers `EMAIL_TAKEN`).
pub async fn retake_unconfirmed_in(
    conn: &mut PgConnection,
    id: Uuid,
    password_hash: &str,
) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE credential SET password_hash = $2, updated_at = greatest(clock_timestamp(), created_at) \
         WHERE id = $1 AND email_verified_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM account_identity WHERE idp_uid = $1::text)",
    )
    .bind(id)
    .bind(password_hash)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 1 {
        spend_all_mail_tokens_in(conn, id).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Marks the address of credential `id` confirmed (once; a later call keeps the first instant).
pub async fn confirm_email_in(conn: &mut PgConnection, id: Uuid) -> Result<bool, IdentityError> {
    let done = sqlx::query(
        "UPDATE credential SET email_verified_at = coalesce(email_verified_at, clock_timestamp()), \
         code_failures = 0 WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// One user of a Firebase Authentication export (`firebase auth:export`), already checked by the
/// caller. `Debug` prints neither the address nor the hash.
#[derive(Clone, PartialEq, Eq)]
pub struct ImportedUser {
    /// The Firebase uid (`localId`): the `account_identity.idp_uid` of its FI account, if any.
    pub firebase_uid: String,
    /// The normalized e-mail.
    pub email_lower: String,
    /// `$firebase-scrypt$<salt>$<hash>`, or `None` (no password: Google-only and the like).
    pub password_hash: Option<String>,
    /// Firebase's `emailVerified`.
    pub email_verified: bool,
}

impl std::fmt::Debug for ImportedUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedUser").finish_non_exhaustive()
    }
}

/// What [`import_in`] did with one user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Imported {
    /// A new credential; `remapped` says whether an FI account's identity now names it.
    New {
        /// The credential id.
        id: Uuid,
        /// An `account_identity` row moved from the Firebase uid to the credential.
        remapped: bool,
    },
    /// The address already has a credential and nothing is orphaned (a rerun, or a Firebase user
    /// without an FI account): untouched.
    Exists,
    /// The address had a local credential with no FI account while the Firebase uid has one
    /// (security review H2): the imported account wins. The credential takes the imported hash
    /// and confirmation, its links are spent, its `updated_at` moves (the holder's ID tokens die),
    /// and the account is re-pointed at it.
    Reclaimed {
        /// The credential id.
        id: Uuid,
    },
    /// Both the local credential and the Firebase uid have FI accounts: nothing changed; the
    /// operator resolves it (reported by Firebase uid, never by address).
    Conflict,
}

/// Re-points the FI account mapped to Firebase uid `uid` at credential `id`.
async fn remap_in(conn: &mut PgConnection, uid: &str, id: Uuid) -> Result<bool, IdentityError> {
    let done = sqlx::query("UPDATE account_identity SET idp_uid = $2 WHERE idp_uid = $1")
        .bind(uid)
        .bind(id.to_string())
        .execute(&mut *conn)
        .await?;
    Ok(done.rows_affected() == 1)
}

/// Imports one Firebase user as a credential (migrator or owner connection; ACCOUNTS.md §4.1
/// "Firebase import"): its hash kept for the first sign-in's rehash (or "password not set"), its
/// confirmation kept, and the FI account mapped to its Firebase uid re-pointed at the credential
/// id so the same account signs in. Idempotent per address; see [`Imported`] for a taken address.
pub async fn import_in(
    conn: &mut PgConnection,
    user: &ImportedUser,
) -> Result<Imported, IdentityError> {
    match insert_in(
        conn,
        &user.email_lower,
        user.password_hash.as_deref(),
        user.email_verified,
    )
    .await?
    {
        Created::New(c) => Ok(Imported::New {
            id: c.id,
            remapped: remap_in(conn, &user.firebase_uid, c.id).await?,
        }),
        Created::EmailTaken => {
            let existing = find_by_email_in(conn, &user.email_lower)
                .await?
                .ok_or(IdentityError::Db(sqlx::Error::RowNotFound))?;
            let local_account = account_of_in(conn, &existing.id.to_string()).await?;
            let firebase_account = account_of_in(conn, &user.firebase_uid).await?;
            match (local_account, firebase_account) {
                (None, Some(_)) => {
                    sqlx::query(
                        "UPDATE credential SET password_hash = $2, \
                         email_verified_at = CASE WHEN $3 THEN clock_timestamp() END, \
                         updated_at = greatest(clock_timestamp(), created_at) WHERE id = $1",
                    )
                    .bind(existing.id)
                    .bind(user.password_hash.as_deref())
                    .bind(user.email_verified)
                    .execute(&mut *conn)
                    .await?;
                    spend_all_mail_tokens_in(conn, existing.id).await?;
                    remap_in(conn, &user.firebase_uid, existing.id).await?;
                    Ok(Imported::Reclaimed { id: existing.id })
                }
                (Some(_), Some(_)) => Ok(Imported::Conflict),
                _ => Ok(Imported::Exists),
            }
        }
    }
}
