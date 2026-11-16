//! Refresh-token families and portal session tokens (ACCOUNTS.md §1, §4.2; ADR 0006 §2).
//!
//! A family is one signed-in launcher install (`client = launcher`, bound to a device) or one
//! portal browser session (`client = portal`). Only SHA-256 lookups are stored; the caller mints the
//! secrets (fi-auth) and passes their hashes.
//!
//! Launcher refresh tokens rotate on every use ([`rotate_in`]): the presented row is locked
//! `FOR UPDATE`, so of two concurrent presenters one rotates and the other sees a used token. A
//! used token presented again revokes the whole family (`reuse_detected`, theft signal) unless it
//! is the same request replayed with the same `Idempotency-Key` within 24 hours (CR-L4): then the
//! successor the client never received is spent and a fresh one minted, never `REFRESH_REUSED`.
//! The successor counts as "never received" only while it is unused: once it was presented, the
//! client had it, and an old request replayed after that is reuse. Lifetime: 90 days on Windows,
//! 30 under Proton or on Linux (`device_binding.platform`, CR-S5), sliding with each rotation.
//!
//! Portal session tokens do not rotate (a browser sends parallel requests): 30 days sliding, 90
//! absolute, checked by lookup on every call so revocation is immediate.

use crate::accounts::{self, AccountState};
use crate::identity::{IdentityError, ms};
use crate::limits::{self, Idempotency, Recorded};
use fi_domain::UnixMs;
use sqlx::{PgConnection, PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

/// Launcher refresh lifetime on Windows.
pub const WINDOWS_REFRESH_DAYS: i32 = 90;
/// Launcher refresh lifetime under Proton or on Linux (CR-S5).
pub const PROTON_REFRESH_DAYS: i32 = 30;
/// Portal session sliding window.
pub const PORTAL_SLIDING_DAYS: i32 = 30;
/// Portal session absolute lifetime.
pub const PORTAL_ABSOLUTE_DAYS: i32 = 90;

/// The refresh lifetime of a device platform, in days.
pub fn refresh_days(platform: &str) -> i32 {
    match platform {
        "windows" => WINDOWS_REFRESH_DAYS,
        _ => PROTON_REFRESH_DAYS,
    }
}

/// A token row just written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IssuedToken {
    /// The row.
    pub token_id: Uuid,
    /// Its family.
    pub family_id: Uuid,
    /// Expiry (database clock).
    pub expires_at: UnixMs,
}

/// Starts a launcher family on `device_binding_id` with the token hashed `token_sha256`.
pub async fn start_launcher_family_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    device_binding_id: Uuid,
    platform: &str,
    token_sha256: &[u8; 32],
) -> Result<IssuedToken, IdentityError> {
    // All fresh launcher families, including sign-in replays, replace the old login. The
    // exclusive account lock also serializes concurrent sign-ins and refresh rotation.
    accounts::lock_account_in(conn, account_id).await?;
    sqlx::query(
        "UPDATE refresh_token SET revoked_at = clock_timestamp(), revoke_reason = 'logout' \
         WHERE account_id = $1 AND client = 'launcher' AND revoked_at IS NULL",
    )
    .bind(account_id)
    .execute(&mut *conn)
    .await?;
    crate::admission::replace_login_in(conn, account_id)
        .await
        .map_err(|e| match e {
            crate::admission::AdmissionError::Db(e) => IdentityError::from(e),
            crate::admission::AdmissionError::Event(e) => IdentityError::Event(e),
            _ => IdentityError::Busy,
        })?;
    let row = sqlx::query(
        "INSERT INTO refresh_token (family_id, account_id, device_binding_id, client, token_sha256, expires_at) \
         VALUES (uuidv7(), $1, $2, 'launcher', $3, clock_timestamp() + make_interval(days => $4)) \
         RETURNING id, family_id, expires_at",
    )
    .bind(account_id)
    .bind(device_binding_id)
    .bind(token_sha256.as_slice())
    .bind(refresh_days(platform))
    .fetch_one(&mut *conn)
    .await?;
    Ok(IssuedToken {
        token_id: row.try_get("id")?,
        family_id: row.try_get("family_id")?,
        expires_at: ms(row.try_get("expires_at")?),
    })
}

/// Opens a portal session (a `client = portal` family of one non-rotating token).
pub async fn start_portal_session_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    token_sha256: &[u8; 32],
) -> Result<IssuedToken, IdentityError> {
    let row = sqlx::query(
        "INSERT INTO refresh_token (family_id, account_id, client, token_sha256, expires_at, last_used_at) \
         VALUES (uuidv7(), $1, 'portal', $2, clock_timestamp() + make_interval(days => $3), clock_timestamp()) \
         RETURNING id, family_id, expires_at",
    )
    .bind(account_id)
    .bind(token_sha256.as_slice())
    .bind(PORTAL_SLIDING_DAYS)
    .fetch_one(&mut *conn)
    .await?;
    Ok(IssuedToken {
        token_id: row.try_get("id")?,
        family_id: row.try_get("family_id")?,
        expires_at: ms(row.try_get("expires_at")?),
    })
}

/// Revokes every live token of `family_id` (sign-out, replay re-issue, admin).
pub async fn revoke_family_in(
    conn: &mut PgConnection,
    family_id: Uuid,
    reason: &str,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "UPDATE refresh_token SET revoked_at = clock_timestamp(), revoke_reason = $2 \
         WHERE family_id = $1 AND revoked_at IS NULL",
    )
    .bind(family_id)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// Revokes every live refresh token and portal session of `account_id` (a `local` password change
/// revokes them with `password_changed`, OD-122 M2.3).
pub async fn revoke_account_in(
    conn: &mut PgConnection,
    account_id: Uuid,
    reason: &str,
) -> Result<u64, IdentityError> {
    let done = sqlx::query(
        "UPDATE refresh_token SET revoked_at = clock_timestamp(), revoke_reason = $2 \
         WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account_id)
    .bind(reason)
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected())
}

/// A family a CR-L4 replay re-issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reissued {
    /// The account, locked by the re-issue (its state now: the caller refuses a ban).
    pub account: AccountState,
    /// The new family's first token.
    pub issued: IssuedToken,
}

/// The CR-L4 re-issue of a sign-in whose answer was lost (`POST /accounts`, `/auth/token`,
/// `/legacy/link`, the portal's sessions): `token_id` is the first token of the family the
/// replayed request issued last (`idempotency_record.resource_id`). Only while that family is
/// live, i.e. no token of it was revoked for any reason (sign-out, reuse, a ban, deletion, a later
/// replay), it is revoked (`replayed`) and a new family starts for the same account, on the same
/// device for a launcher family, as a new portal session otherwise. `None` when the token is
/// unknown or its family is dead: not a replay, the caller answers its refusal. The account row is
/// locked first (the order of every account write), so two replays of one family serialize there
/// and the second finds it revoked and re-issues nothing.
pub async fn reissue_family_in(
    conn: &mut PgConnection,
    token_id: Uuid,
    token_sha256: &[u8; 32],
) -> Result<Option<Reissued>, IdentityError> {
    let row = sqlx::query(
        "SELECT t.account_id, t.family_id, t.client, t.device_binding_id, d.platform FROM refresh_token t \
         LEFT JOIN device_binding d ON d.id = t.device_binding_id WHERE t.id = $1",
    )
    .bind(token_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let account_id: Uuid = row.try_get("account_id")?;
    let family_id: Uuid = row.try_get("family_id")?;
    let client: String = row.try_get("client")?;
    let binding: Option<Uuid> = row.try_get("device_binding_id")?;
    let platform: Option<String> = row.try_get("platform")?;
    let account = accounts::lock_account_in(conn, account_id).await?;
    if revoke_family_in(conn, family_id, "replayed").await? == 0 {
        return Ok(None);
    }
    let issued = match (client.as_str(), binding, platform) {
        ("launcher", Some(binding), Some(platform)) => {
            start_launcher_family_in(conn, account_id, binding, &platform, token_sha256).await?
        }
        _ => start_portal_session_in(conn, account_id, token_sha256).await?,
    };
    Ok(Some(Reissued { account, issued }))
}

/// [`reissue_family_in`] for the request `(scope, idem)`, whose record is then re-pointed at the
/// new family's token in the same transaction: the next replay revokes this family, so at most one
/// family lives per `(scope, key)` however often the request is replayed. Take
/// [`limits::lock_key_in`] first.
pub async fn replay_family_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
    token_id: Uuid,
    token_sha256: &[u8; 32],
) -> Result<Option<Reissued>, IdentityError> {
    let Some(reissued) = reissue_family_in(conn, token_id, token_sha256).await? else {
        return Ok(None);
    };
    limits::repoint_in(conn, scope, idem, reissued.issued.token_id).await?;
    Ok(Some(reissued))
}

/// The result of presenting a refresh token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rotation {
    /// A successor was minted (`replayed`: the CR-L4 replay path).
    Rotated {
        /// The account.
        account_id: Uuid,
        /// The family (unchanged).
        family_id: Uuid,
        /// `player` | `moderator`.
        role: String,
        /// The successor row.
        token: IssuedToken,
        /// Whether this answered a replay.
        replayed: bool,
    },
    /// Unknown, revoked, expired or not a launcher token: `AUTH.REFRESH_INVALID`.
    Invalid,
    /// A used token presented again: the family is now revoked (`AUTH.REFRESH_REUSED`). The caller
    /// commits, so the revocation stays.
    Reused,
    /// The account is banned (`ACCOUNTS.BANNED`), with its review (`account.ban_review`) when the
    /// ban is a block pending review (CR-S52).
    Banned {
        /// `seed_from_profile`, or `None` for an ordinary ban.
        review: Option<String>,
    },
    /// The key was used for another request (`REQUEST.IDEMPOTENCY_KEY_REUSED`).
    KeyReused,
}

/// Presents the launcher refresh token hashed `presented`; on success its successor is
/// `successor_sha256`. `idem` is this request's `Idempotency-Key`, scope `account:<id>` (known once
/// the token is found). Always commit the caller's transaction, whatever the outcome.
pub async fn rotate_in(
    conn: &mut PgConnection,
    presented: &[u8; 32],
    successor_sha256: &[u8; 32],
    idem: &Idempotency,
) -> Result<Rotation, IdentityError> {
    // Account before token row, matching sign-in. Without this, rotation could insert a new
    // unrevoked successor after a concurrent login had revoked its predecessor.
    let account: Option<Uuid> =
        sqlx::query_scalar("SELECT account_id FROM refresh_token WHERE token_sha256 = $1")
            .bind(presented.as_slice())
            .fetch_optional(&mut *conn)
            .await?;
    let Some(account) = account else {
        return Ok(Rotation::Invalid);
    };
    accounts::lock_account_in(conn, account).await?;
    let row = sqlx::query(
        "SELECT t.id, t.family_id, t.account_id, t.client, t.device_binding_id, t.expires_at, t.used_at, \
                t.replaced_by, t.revoked_at, a.status, (a.banned_at IS NOT NULL AND (a.banned_until IS NULL OR a.banned_until > clock_timestamp())) AS banned, CASE WHEN a.banned_at IS NOT NULL AND (a.banned_until IS NULL OR a.banned_until > clock_timestamp()) THEN a.ban_review END AS ban_review, a.role, \
                d.platform, clock_timestamp() AS now \
         FROM refresh_token t JOIN account a ON a.id = t.account_id \
         LEFT JOIN device_binding d ON d.id = t.device_binding_id \
         WHERE t.token_sha256 = $1 FOR UPDATE OF t",
    )
    .bind(presented.as_slice())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(Rotation::Invalid);
    };
    let token_id: Uuid = row.try_get("id")?;
    let family_id: Uuid = row.try_get("family_id")?;
    let account_id: Uuid = row.try_get("account_id")?;
    let client: String = row.try_get("client")?;
    let binding: Option<Uuid> = row.try_get("device_binding_id")?;
    let platform: Option<String> = row.try_get("platform")?;
    let status: String = row.try_get("status")?;
    let role: String = row.try_get("role")?;
    let now = ms(row.try_get("now")?);
    let expires_at = ms(row.try_get("expires_at")?);
    let used = row
        .try_get::<Option<OffsetDateTime>, _>("used_at")?
        .is_some();
    let revoked = row
        .try_get::<Option<OffsetDateTime>, _>("revoked_at")?
        .is_some();
    let (Some(binding), Some(platform)) = (binding, platform) else {
        return Ok(Rotation::Invalid);
    };
    if client != "launcher" || revoked || status == "deleted" {
        return Ok(Rotation::Invalid);
    }
    let scope = format!("account:{account_id}");
    if used {
        // A replay of the request that rotated it, or reuse.
        match limits::recorded_in(conn, &scope, idem).await? {
            Recorded::Same(record) if record.resource_id == Some(token_id) => {
                let successor: Option<Uuid> = row.try_get("replaced_by")?;
                let live = match successor {
                    Some(s) => sqlx::query_scalar::<_, bool>(
                        "SELECT used_at IS NULL AND revoked_at IS NULL FROM refresh_token WHERE id = $1 FOR UPDATE",
                    )
                    .bind(s)
                    .fetch_optional(&mut *conn)
                    .await?
                    .unwrap_or(false),
                    None => false,
                };
                if let (true, Some(never_received)) = (live, successor) {
                    if row.try_get::<bool, _>("banned")? {
                        return Ok(Rotation::Banned {
                            review: row.try_get("ban_review")?,
                        });
                    }
                    let token = insert_successor(
                        conn,
                        family_id,
                        account_id,
                        binding,
                        &platform,
                        successor_sha256,
                    )
                    .await?;
                    // The never-received successor is spent (presenting it later is reuse).
                    sqlx::query(
                        "UPDATE refresh_token SET used_at = clock_timestamp(), replaced_by = $2 WHERE id = $1",
                    )
                    .bind(never_received)
                    .bind(token.token_id)
                    .execute(&mut *conn)
                    .await?;
                    sqlx::query("UPDATE refresh_token SET replaced_by = $2 WHERE id = $1")
                        .bind(token_id)
                        .bind(token.token_id)
                        .execute(&mut *conn)
                        .await?;
                    return Ok(Rotation::Rotated {
                        account_id,
                        family_id,
                        role,
                        token,
                        replayed: true,
                    });
                }
            }
            Recorded::Same(_) | Recorded::Different => return Ok(Rotation::KeyReused),
            Recorded::None => {}
        }
        revoke_family_in(conn, family_id, "reuse_detected").await?;
        return Ok(Rotation::Reused);
    }
    if expires_at <= now {
        return Ok(Rotation::Invalid);
    }
    if row.try_get::<bool, _>("banned")? {
        return Ok(Rotation::Banned {
            review: row.try_get("ban_review")?,
        });
    }
    match limits::recorded_in(conn, &scope, idem).await? {
        Recorded::None => {}
        // The key names another request (another token, or another route).
        Recorded::Same(_) | Recorded::Different => return Ok(Rotation::KeyReused),
    }
    let token = insert_successor(
        conn,
        family_id,
        account_id,
        binding,
        &platform,
        successor_sha256,
    )
    .await?;
    sqlx::query(
        "UPDATE refresh_token SET used_at = clock_timestamp(), last_used_at = clock_timestamp(), replaced_by = $2 WHERE id = $1",
    )
    .bind(token_id)
    .bind(token.token_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE device_binding SET last_seen_at = clock_timestamp() WHERE id = $1 AND unbound_at IS NULL",
    )
    .bind(binding)
    .execute(&mut *conn)
    .await?;
    limits::record_in(conn, &scope, idem, 200, Some(token_id)).await?;
    Ok(Rotation::Rotated {
        account_id,
        family_id,
        role,
        token,
        replayed: false,
    })
}

async fn insert_successor(
    conn: &mut PgConnection,
    family_id: Uuid,
    account_id: Uuid,
    binding: Uuid,
    platform: &str,
    token_sha256: &[u8; 32],
) -> Result<IssuedToken, IdentityError> {
    let row = sqlx::query(
        "INSERT INTO refresh_token (family_id, account_id, device_binding_id, client, token_sha256, expires_at) \
         VALUES ($1, $2, $3, 'launcher', $4, clock_timestamp() + make_interval(days => $5)) \
         RETURNING id, family_id, expires_at",
    )
    .bind(family_id)
    .bind(account_id)
    .bind(binding)
    .bind(token_sha256.as_slice())
    .bind(refresh_days(platform))
    .fetch_one(&mut *conn)
    .await?;
    Ok(IssuedToken {
        token_id: row.try_get("id")?,
        family_id: row.try_get("family_id")?,
        expires_at: ms(row.try_get("expires_at")?),
    })
}

/// A live portal session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalSession {
    /// The account.
    pub account_id: Uuid,
    /// The session's family (what `DELETE /sessions/current` revokes).
    pub family_id: Uuid,
    /// Sliding expiry after this use.
    pub expires_at: UnixMs,
}

/// Looks up a portal session token and slides its expiry (at most once a minute per session, and
/// never past 90 days after it was opened). `None`: missing, expired or revoked.
pub async fn portal_session(
    pool: &PgPool,
    token_sha256: &[u8; 32],
) -> Result<Option<PortalSession>, IdentityError> {
    let row = sqlx::query(
        "UPDATE refresh_token SET last_used_at = clock_timestamp(), \
           expires_at = LEAST(clock_timestamp() + make_interval(days => $2), issued_at + make_interval(days => $3)) \
         WHERE token_sha256 = $1 AND client = 'portal' AND revoked_at IS NULL AND expires_at > clock_timestamp() \
           AND (last_used_at IS NULL OR last_used_at < clock_timestamp() - interval '1 minute') \
         RETURNING account_id, family_id, expires_at",
    )
    .bind(token_sha256.as_slice())
    .bind(PORTAL_SLIDING_DAYS)
    .bind(PORTAL_ABSOLUTE_DAYS)
    .fetch_optional(pool)
    .await?;
    let row = match row {
        Some(row) => Some(row),
        None => {
            sqlx::query(
                "SELECT account_id, family_id, expires_at FROM refresh_token \
                 WHERE token_sha256 = $1 AND client = 'portal' AND revoked_at IS NULL AND expires_at > clock_timestamp()",
            )
            .bind(token_sha256.as_slice())
            .fetch_optional(pool)
            .await?
        }
    };
    row.map(|r| {
        Ok(PortalSession {
            account_id: r.try_get("account_id")?,
            family_id: r.try_get("family_id")?,
            expires_at: ms(r.try_get("expires_at")?),
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// Whether `family_id` of `account_id` still has a live token (not revoked): the bearer check of
/// launcher access tokens, so sign-out, reuse detection and deletion take effect at once.
pub async fn family_live(
    pool: &PgPool,
    account_id: Uuid,
    family_id: Uuid,
) -> Result<bool, IdentityError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM refresh_token WHERE family_id = $1 AND account_id = $2 AND revoked_at IS NULL)",
    )
    .bind(family_id)
    .bind(account_id)
    .fetch_one(pool)
    .await?)
}
