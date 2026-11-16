//! The MCP grant store (ADR 0018 §3; tables `mcp_client`, `mcp_request`, `mcp_connection`,
//! `mcp_refresh`). This run builds the read every `/mcp` call makes after the access token
//! verified: the connection must be live, its account not banned or deleted, and the account's
//! role unchanged since consent. The OAuth writes (authorize, consent, code, token rotation) land
//! with the flow (milestone PV1-N).

use crate::identity::IdentityError;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

/// A live connection as `/mcp` sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveConnection {
    /// The account.
    pub account_id: Uuid,
    /// The client it was granted to.
    pub client_id: String,
    /// The role at consent (equal to the account's role now).
    pub role: String,
    /// Granted scopes (`platform:read`, `moderation:write`).
    pub scopes: Vec<String>,
}

/// The connection `id` of `account_id` when it is usable now: not revoked, not past its 30-day
/// life, the account active or pending deletion, not banned, and still holding the role it had at
/// consent (a demoted moderator's connection dies at once). `None` otherwise, without saying why.
pub async fn live_connection_in(
    conn: &mut PgConnection,
    id: Uuid,
    account_id: Uuid,
) -> Result<Option<LiveConnection>, IdentityError> {
    let row = sqlx::query(
        "SELECT c.account_id, c.client_id, c.role, c.scopes FROM mcp_connection c JOIN account a ON a.id = c.account_id \
         WHERE c.id = $1 AND c.account_id = $2 AND c.revoked_at IS NULL AND c.expires_at > clock_timestamp() \
           AND a.status <> 'deleted' AND a.role = c.role \
           AND NOT (a.banned_at IS NOT NULL AND (a.banned_until IS NULL OR a.banned_until > clock_timestamp()))",
    )
    .bind(id)
    .bind(account_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|r| {
        Ok(LiveConnection {
            account_id: r.try_get("account_id")?,
            client_id: r.try_get("client_id")?,
            role: r.try_get("role")?,
            scopes: r.try_get("scopes")?,
        })
    })
    .transpose()
    .map_err(|e: sqlx::Error| e.into())
}

/// Marks the connection used (`authenticated_at`), at most once a minute per connection.
pub async fn touch_in(conn: &mut PgConnection, id: Uuid) -> Result<(), IdentityError> {
    sqlx::query(
        "UPDATE mcp_connection SET authenticated_at = clock_timestamp() \
         WHERE id = $1 AND authenticated_at < clock_timestamp() - interval '1 minute'",
    )
    .bind(id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
