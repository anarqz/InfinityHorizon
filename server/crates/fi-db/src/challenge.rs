//! Turnstile tokens already presented (ADR 0903 §2, table `challenge_use`): a token opens one
//! request, ever. Core spends the token here **before** asking Cloudflare's siteverify, so two
//! concurrent requests with one token cannot both pass, and the network call happens outside any
//! transaction (services/AGENTS.md).

use crate::identity::IdentityError;
use sqlx::PgConnection;

/// How long a spent token is remembered (Cloudflare refuses a token older than 300 s anyway).
pub const REMEMBER_MINUTES: i32 = 10;
/// Expired rows the inserting statement deletes at most (the table stays small without a worker).
pub const PRUNE_BATCH: i64 = 100;

/// Records the token's SHA-256; `true` the first time, `false` when it was presented in the last
/// [`REMEMBER_MINUTES`] minutes (a replay: `AUTH.CHALLENGE_FAILED`). One statement, also pruning a
/// bounded batch of expired rows.
pub async fn spend_in(
    conn: &mut PgConnection,
    token_sha256: &[u8; 32],
) -> Result<bool, IdentityError> {
    let first: Option<i32> = sqlx::query_scalar(
        "WITH pruned AS ( \
           DELETE FROM challenge_use WHERE token_sha256 IN ( \
             SELECT token_sha256 FROM challenge_use \
             WHERE used_at < clock_timestamp() - make_interval(mins => $2) ORDER BY used_at LIMIT $3)) \
         INSERT INTO challenge_use (token_sha256) VALUES ($1) ON CONFLICT DO NOTHING RETURNING 1",
    )
    .bind(token_sha256.as_slice())
    .bind(REMEMBER_MINUTES)
    .bind(PRUNE_BATCH)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(first.is_some())
}
