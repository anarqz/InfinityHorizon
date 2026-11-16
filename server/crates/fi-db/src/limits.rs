//! Shared rate limits and idempotency records (ACCOUNTS.md §1, §9; tables of the accounts
//! migration).
//!
//! **Rate limits** are one upsert per request on `rate_limit(key, window_start)`, fixed windows
//! aligned to multiples of `window_s` since the epoch, counted in Postgres so every instance sees
//! the same requests. Keys are `<limit>:<hex HMAC>` (fi-auth's pepper), never a subject in clear;
//! a subject that is not personal data and exists where no pepper is configured (the game plane's
//! per-session gamertag lookups, [`hit_n_in`]) is keyed by the plain SHA-256 of its id instead.
//! [`hit`] counts in its own statement (sign-in attempts must count even when they fail);
//! [`hit_in`] counts inside the caller's transaction, so only what commits counts and concurrent
//! requests on one key serialize on its row (account creation: "10 per network per day" is exact
//! across instances).
//!
//! **Idempotency records** remember, per `(scope, key)`, which request a key was used for (route
//! and SHA-256 of the canonical body) and what it produced (`resource_id`, a `response` without
//! secrets), for 24 hours. Responses carrying a secret are never stored: their replay re-issues
//! the secret, revokes the one the record names and re-points the record at the new one
//! ([`repoint_in`], the only column that may change), so at most one issued secret lives per
//! `(scope, key)` (CR-L4). Routes whose replay re-issues a secret take [`lock_key_in`] first: two
//! requests with one key serialize, and the second sees the first's committed record.

use crate::identity::IdentityError;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// A limit: at most `max` requests per `window_s` seconds per key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    /// The key prefix (`signin.uid`, `create.net`, ...).
    pub name: &'static str,
    /// Requests allowed per window.
    pub max: i32,
    /// Window length (1..=86400).
    pub window_s: i32,
}

/// Sign-in exchanges per identity uid (10 per 15 minutes).
pub const SIGNIN_UID: Limit = Limit {
    name: "signin.uid",
    max: 10,
    window_s: 900,
};
/// Sign-in exchanges per client network (30 per 15 minutes).
pub const SIGNIN_NET: Limit = Limit {
    name: "signin.net",
    max: 30,
    window_s: 900,
};
/// Legacy links per gamertag (10 per 15 minutes).
pub const LEGACY_GAMERTAG: Limit = Limit {
    name: "legacy.gamertag",
    max: 10,
    window_s: 900,
};
/// Legacy links per client network (30 per 15 minutes).
pub const LEGACY_NET: Limit = Limit {
    name: "legacy.net",
    max: 30,
    window_s: 900,
};
/// Account creations per device (3 per day).
pub const CREATE_DEVICE: Limit = Limit {
    name: "create.device",
    max: 3,
    window_s: 86_400,
};
/// Account creations per client network (10 per day).
pub const CREATE_NET: Limit = Limit {
    name: "create.net",
    max: 10,
    window_s: 86_400,
};
/// `local` sign-ups, sign-ins and password changes per e-mail address (10 per 15 minutes; OD-122,
/// ACCOUNTS.md §4.1 "Two identity backends"), counted whether or not the address has a credential.
pub const LOCAL_EMAIL: Limit = Limit {
    name: "local.email",
    max: 10,
    window_s: 900,
};
/// `local` confirmation mails resent per credential (3 per hour; CR-S53).
pub const LOCAL_MAIL_CREDENTIAL: Limit = Limit {
    name: "local.mail.credential",
    max: 3,
    window_s: 3_600,
};
/// `local` password mails asked per e-mail address (3 per hour; CR-S53), counted whether or not
/// the address has a credential.
pub const LOCAL_MAIL_ADDRESS: Limit = Limit {
    name: "local.mail.address",
    max: 3,
    window_s: 3_600,
};
/// `local` mail requests (resend, reset) and mail-page submissions per client network (10 per
/// hour; CR-S53).
pub const LOCAL_MAIL_NET: Limit = Limit {
    name: "local.mail.net",
    max: 10,
    window_s: 3_600,
};
/// `local` confirmation-code submissions per credential (10 per 15 minutes; CR-S53 follow-up), on
/// top of the 5 wrong codes that spend one code.
pub const LOCAL_CODE_CREDENTIAL: Limit = Limit {
    name: "local.code.credential",
    max: 10,
    window_s: 900,
};
/// `local` confirmation-code submissions per client network (30 per 15 minutes).
pub const LOCAL_CODE_NET: Limit = Limit {
    name: "local.code.net",
    max: 30,
    window_s: 900,
};
/// Wrong `local` confirmation codes per credential (10 per 24 hours; security re-review M-C1):
/// counted only on wrong codes, checked first with [`peek_in`].
pub const LOCAL_CODE_WRONG: Limit = Limit {
    name: "local.code.wrong",
    max: 10,
    window_s: 86_400,
};
/// A save line's commits per hour (SAVES_API.md §4.3): 30 per profile per clock hour, the
/// `maxCommitsPerHour` `/launcher/v1/info` advertises by default (the save routes run without the
/// identity configuration, so the number is fixed here).
pub const SAVE_COMMITS: Limit = Limit {
    name: "saves.commit",
    max: 30,
    window_s: 3600,
};

/// Upload dry runs per account (CR-S71, SAVES_API.md §4.11): 10 per clock hour, counted before the
/// bundle is verified whatever the answer.
pub const SAVE_UPLOAD_CHECKS: Limit = Limit {
    name: "saves.upload_check",
    max: 10,
    window_s: 3600,
};

/// Presence reads per account (CR-S72, ACCOUNTS.md §5 "Presence"): 60 per clock hour (the
/// launcher reads at game start and every 5 minutes in game).
pub const PRESENCE_READS: Limit = Limit {
    name: "launcher.presence",
    max: 60,
    window_s: 3600,
};

/// The `rate_limit` key of an account under `limit`: the plain SHA-256 of its id (the save surface
/// runs where no pepper is configured; an account id alone names no person).
pub fn account_key(limit: &Limit, account: Uuid) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("account:{account}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{hex}", limit.name)
}

/// The `rate_limit` key of a profile under `limit`: the plain SHA-256 of its id (a profile id is
/// no personal data, and the save surface runs where no pepper is configured).
pub fn profile_key(limit: &Limit, profile: Uuid) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(format!("profile:{profile}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}:{hex}", limit.name)
}

/// The answer of a limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Under the limit (this request counted).
    Allowed,
    /// Over it: retry after this many seconds (`Retry-After`).
    Limited {
        /// Seconds until the window ends.
        retry_after_secs: i64,
    },
}

const UPSERT: &str = "INSERT INTO rate_limit (key, window_start, window_s, count) \
     VALUES ($1, to_timestamp(floor(extract(epoch FROM clock_timestamp()) / $2) * $2), $2, 1) \
     ON CONFLICT (key, window_start) DO UPDATE SET count = rate_limit.count + 1 \
     RETURNING count, ceil(extract(epoch FROM (window_start + make_interval(secs => window_s) - clock_timestamp())))::bigint AS remaining";

fn answer(row: &sqlx::postgres::PgRow, limit: &Limit) -> Result<Hit, sqlx::Error> {
    let count: i32 = row.try_get("count")?;
    if count > limit.max {
        let remaining: i64 = row.try_get("remaining")?;
        Ok(Hit::Limited {
            retry_after_secs: remaining.max(1),
        })
    } else {
        Ok(Hit::Allowed)
    }
}

/// Counts one request on `key` in its own statement.
pub async fn hit(pool: &PgPool, key: &str, limit: &Limit) -> Result<Hit, IdentityError> {
    let row = sqlx::query(UPSERT)
        .bind(key)
        .bind(limit.window_s)
        .fetch_one(pool)
        .await?;
    Ok(answer(&row, limit)?)
}

/// Counts one request on `key` inside the caller's transaction (kept only if it commits).
pub async fn hit_in(
    conn: &mut PgConnection,
    key: &str,
    limit: &Limit,
) -> Result<Hit, IdentityError> {
    let row = sqlx::query(UPSERT)
        .bind(key)
        .bind(limit.window_s)
        .fetch_one(&mut *conn)
        .await?;
    Ok(answer(&row, limit)?)
}

/// Whether `key` already spent `limit` in the current window, without counting anything (a
/// limit counted only on failures, such as wrong world codes, is checked first with this).
pub async fn peek_in(
    conn: &mut PgConnection,
    key: &str,
    limit: &Limit,
) -> Result<Hit, IdentityError> {
    let row: Option<(i32, i64)> = sqlx::query_as(
        "SELECT count, ceil(extract(epoch FROM (window_start + make_interval(secs => window_s) - clock_timestamp())))::bigint \
         FROM rate_limit WHERE key = $1 \
           AND window_start = to_timestamp(floor(extract(epoch FROM clock_timestamp()) / $2) * $2)",
    )
    .bind(key)
    .bind(limit.window_s)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match row {
        Some((count, remaining)) if count >= limit.max => Hit::Limited {
            retry_after_secs: remaining.max(1),
        },
        _ => Hit::Allowed,
    })
}

/// Counts `n` units (not one request) on `key` inside the caller's connection: what a limit on
/// looked-up items rather than on requests needs (the game plane's gamertag lookups, GAME_PLATFORM.md
/// §3a; `n` at least 1, as the table's CHECK wants). Limited when the window's count passes
/// `limit.max`.
pub async fn hit_n_in(
    conn: &mut PgConnection,
    key: &str,
    limit: &Limit,
    n: i32,
) -> Result<Hit, IdentityError> {
    let row = sqlx::query(
        "INSERT INTO rate_limit (key, window_start, window_s, count) \
         VALUES ($1, to_timestamp(floor(extract(epoch FROM clock_timestamp()) / $2) * $2), $2, $3) \
         ON CONFLICT (key, window_start) DO UPDATE SET count = rate_limit.count + $3 \
         RETURNING count, ceil(extract(epoch FROM (window_start + make_interval(secs => window_s) - clock_timestamp())))::bigint AS remaining",
    )
    .bind(key)
    .bind(limit.window_s)
    .bind(n.max(1))
    .fetch_one(&mut *conn)
    .await?;
    Ok(answer(&row, limit)?)
}

/// One request's idempotency identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Idempotency {
    /// The client's `Idempotency-Key`.
    pub key: Uuid,
    /// `POST /launcher/v1/accounts` style.
    pub route: String,
    /// SHA-256 of the canonical request body.
    pub request_sha256: [u8; 32],
}

/// What a first request recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// HTTP status it answered.
    pub status: i16,
    /// What it created.
    pub resource_id: Option<Uuid>,
    /// Its body, when it carried no secret.
    pub response: Option<serde_json::Value>,
}

/// Whether `(scope, key)` was used in the last 24 hours, and for what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// Never (or longer ago).
    None,
    /// For this same request (route and body hash).
    Same(Record),
    /// For another request: `REQUEST.IDEMPOTENCY_KEY_REUSED`.
    Different,
}

/// Looks `(scope, key)` up.
pub async fn recorded_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
) -> Result<Recorded, IdentityError> {
    let row = sqlx::query(
        "SELECT route, request_sha256, status, resource_id, response FROM idempotency_record \
         WHERE scope = $1 AND key = $2 AND created_at > clock_timestamp() - interval '24 hours'",
    )
    .bind(scope)
    .bind(idem.key)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else {
        return Ok(Recorded::None);
    };
    let route: String = row.try_get("route")?;
    let hash: Vec<u8> = row.try_get("request_sha256")?;
    if route != idem.route || hash.as_slice() != idem.request_sha256.as_slice() {
        return Ok(Recorded::Different);
    }
    Ok(Recorded::Same(Record {
        status: row.try_get("status")?,
        resource_id: row.try_get("resource_id")?,
        response: row.try_get("response")?,
    }))
}

/// First key of the two-key advisory locks of idempotency keys ("FIK0"; pool admission's is
/// "FIP0", [`crate::sessions::POOL_LOCK_NAMESPACE`]).
pub const KEY_LOCK_NAMESPACE: i32 = 0x4649_4b30;

/// Serializes the requests that carry `(scope, key)` until the caller's transaction ends: a
/// concurrent duplicate waits for the first to commit, then [`recorded_in`] (a later statement,
/// so a later snapshot) finds its record and the duplicate replays it instead of failing on the
/// insert. Take it as the first lock of the transaction (it then never closes a cycle). Waits at
/// most the connection's `lock_timeout`, then fails as [`IdentityError::Busy`] (retryable).
pub async fn lock_key_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
) -> Result<(), IdentityError> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(KEY_LOCK_NAMESPACE)
        .bind(format!("{scope} {}", idem.key))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Re-points the record of `(scope, key)` at `resource_id`, what a replay just issued in its place
/// (CR-L4: the next replay then revokes the newest secret, never an older one). Only this column
/// may change (the table's trigger). [`IdentityError::IdempotencyKeyReused`] when the record is
/// gone or names another request: the caller rolls back, nothing is re-issued.
pub async fn repoint_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
    resource_id: Uuid,
) -> Result<(), IdentityError> {
    let done = sqlx::query(
        "UPDATE idempotency_record SET resource_id = $3 \
         WHERE scope = $1 AND key = $2 AND route = $4 AND request_sha256 = $5",
    )
    .bind(scope)
    .bind(idem.key)
    .bind(resource_id)
    .bind(&idem.route)
    .bind(idem.request_sha256.as_slice())
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() != 1 {
        return Err(IdentityError::IdempotencyKeyReused);
    }
    Ok(())
}

/// Records a first request's outcome in the caller's transaction. A concurrent request that
/// recorded the same key first makes this fail with [`IdentityError::IdempotencyKeyReused`] (the
/// caller rolls back; its retry then finds the record).
pub async fn record_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
    status: i16,
    resource_id: Option<Uuid>,
) -> Result<(), IdentityError> {
    record_with_response_in(conn, scope, idem, status, resource_id, None).await
}

/// [`record_in`] with a response body (never one that carries a secret).
pub async fn record_with_response_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
    status: i16,
    resource_id: Option<Uuid>,
    response: Option<&serde_json::Value>,
) -> Result<(), IdentityError> {
    let done = sqlx::query(
        "INSERT INTO idempotency_record (scope, key, route, request_sha256, status, resource_id, response) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (scope, key) DO NOTHING",
    )
    .bind(scope)
    .bind(idem.key)
    .bind(&idem.route)
    .bind(idem.request_sha256.as_slice())
    .bind(status)
    .bind(resource_id)
    .bind(response)
    .execute(&mut *conn)
    .await?;
    if done.rows_affected() == 0 {
        return Err(IdentityError::IdempotencyKeyReused);
    }
    Ok(())
}
