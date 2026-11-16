//! What the launcher and portal identity routes share: input checks, the Identity Platform
//! exchange (ID token → [`IdentityClaim`], before any transaction: no network inside one), rate
//! limits, and issuing token families.

use crate::errors::ApiError;
use crate::extract::ClientNetwork;
use crate::state::{AppState, now_secs};
use crate::views;
use fi_auth::Secret;
use fi_auth::access::{self, Audience};
use fi_auth::idtoken::{IdTokenError, Identity};
use fi_auth::opaque::{self, Kind};
use fi_db::accounts::{self, NewDevice};
use fi_db::identity::{self, IdentityClaim};
use fi_db::limits::{self, Hit, Idempotency, Limit};
use fi_db::tokens::{self, IssuedToken};
use fi_domain::UnixMs;
use fi_protocol::ErrorCode;
use fi_protocol::accounts::Device;
use sqlx::PgConnection;
use uuid::Uuid;

/// Rejects new registration without affecting existing-account sign-in or cleanup: both the
/// deployment flag and the database switch must be open.
pub async fn require_registration(
    state: &AppState,
    conn: &mut PgConnection,
) -> Result<(), ApiError> {
    if registration_open(state, conn).await? {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorCode::AccountsRegistrationDisabled,
            "registrations disabled",
        ))
    }
}

/// Whether new accounts may be created now.
pub async fn registration_open(
    state: &AppState,
    conn: &mut PgConnection,
) -> Result<bool, ApiError> {
    Ok(state.id().info.registration_enabled && fi_db::registration::open_in(conn).await?)
}

/// Checks a `Device` body (ACCOUNTS.md §5): a non-nil UUID, a label of 1-64 characters without
/// control characters, a known platform, and an optional fingerprint of 43 base64url characters
/// (CR-S44), kept only as its HMAC with the pepper.
pub fn device(state: &AppState, d: &Device) -> Result<NewDevice, ApiError> {
    let device_id = Uuid::parse_str(&d.device_id)
        .ok()
        .filter(|u| !u.is_nil())
        .ok_or_else(|| ApiError::invalid("device.deviceId", "a UUID"))?;
    let label = d.label.trim();
    if label.is_empty() || label.chars().count() > 64 || label.chars().any(char::is_control) {
        return Err(ApiError::invalid("device.label", "1 to 64 characters"));
    }
    if !matches!(d.platform.as_str(), "windows" | "proton" | "linux") {
        return Err(ApiError::invalid(
            "device.platform",
            "windows, proton or linux",
        ));
    }
    let fingerprint_hmac = match d.fingerprint.as_deref() {
        None => None,
        Some(f)
            if f.len() == 43
                && f.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
        {
            Some(state.id().pepper()?.device_fingerprint(f))
        }
        Some(_) => {
            return Err(ApiError::invalid(
                "device.fingerprint",
                "43 base64url characters",
            ));
        }
    };
    Ok(NewDevice {
        device_id,
        label: label.to_owned(),
        platform: d.platform.clone(),
        fingerprint_hmac,
    })
}

/// Verifies an ID token at this instance's clock: the instance's own (`FI_AUTH_BACKEND=local`,
/// [`fi_auth::localidp`]) or Identity Platform's, never both. Either answers the same claim, so the
/// exchange, deletion reauth and the portal are one code path for both backends.
pub async fn verify_id_token(state: &AppState, id_token: &str) -> Result<IdentityClaim, ApiError> {
    if let Some(local) = &state.id().local_idp {
        return claim_of(local.verify(id_token, now_secs()));
    }
    let dynamic;
    let verifier = if let Some(remote) = &state.id().jwks {
        dynamic = remote
            .verifier_for(id_token)
            .await
            .map_err(|error| match error {
                crate::jwks::RefreshError::Invalid => {
                    ApiError::new(ErrorCode::AuthIdTokenInvalid, "ID token refused")
                }
                crate::jwks::RefreshError::Unavailable => ApiError::unavailable(),
            })?;
        &dynamic
    } else {
        state.id().verifier()?
    };
    claim_of(verifier.verify(id_token, now_secs()))
}

fn claim_of(verified: Result<Identity, IdTokenError>) -> Result<IdentityClaim, ApiError> {
    let identity = verified.map_err(|e| match e {
        IdTokenError::Invalid => ApiError::new(ErrorCode::AuthIdTokenInvalid, "ID token refused"),
        IdTokenError::ProviderNotAllowed => ApiError::new(
            ErrorCode::AuthProviderNotAllowed,
            "sign-in provider not allowed",
        ),
        IdTokenError::EmailUnverified => {
            ApiError::new(ErrorCode::AuthEmailUnverified, "e-mail not verified")
        }
    })?;
    Ok(IdentityClaim {
        email_lower: identity.email_lower(),
        idp_uid: identity.uid,
        provider_id: identity.provider_id,
        email_verified: identity.email_verified,
        auth_time: UnixMs::from_millis(identity.auth_time.saturating_mul(1_000)),
    })
}

fn limited(hit: Hit, code: ErrorCode) -> Result<(), ApiError> {
    match hit {
        Hit::Allowed => Ok(()),
        Hit::Limited { retry_after_secs } => {
            Err(ApiError::new(code, "too many requests, wait Retry-After")
                .retry_after(retry_after_secs.max(1) as u64))
        }
    }
}

/// Counts one request on `subject` under `limit` in its own statement (attempts count even when
/// the request then fails).
pub async fn limit(
    state: &AppState,
    limit: &Limit,
    subject: &str,
    code: ErrorCode,
) -> Result<(), ApiError> {
    let key = state.id().pepper()?.rate_limit_key(limit.name, subject);
    let limit = state.id().scaled(limit);
    limited(limits::hit(&state.pool, &key, &limit).await?, code)
}

/// [`limit`] inside the caller's transaction (only what commits counts).
pub async fn limit_in(
    conn: &mut PgConnection,
    state: &AppState,
    limit: &Limit,
    subject: &str,
    code: ErrorCode,
) -> Result<(), ApiError> {
    let key = state.id().pepper()?.rate_limit_key(limit.name, subject);
    let limit = state.id().scaled(limit);
    limited(limits::hit_in(conn, &key, &limit).await?, code)
}

/// The sign-in exchange limits: per identity uid and per client network.
pub async fn exchange_limits(
    state: &AppState,
    claim: &IdentityClaim,
    net: &ClientNetwork,
) -> Result<(), ApiError> {
    limit(
        state,
        &limits::SIGNIN_UID,
        &format!("uid:{}", claim.idp_uid),
        ErrorCode::AuthRateLimited,
    )
    .await?;
    limit(
        state,
        &limits::SIGNIN_NET,
        &net.0,
        ErrorCode::AuthRateLimited,
    )
    .await
}

/// A launcher sign-in's tokens.
#[derive(Debug)]
pub struct LauncherTokens {
    /// The account.
    pub account_id: Uuid,
    /// The new refresh token (shown once).
    pub refresh: Secret,
    /// Its row (`idempotency_record.resource_id` of the request).
    pub issued: IssuedToken,
    /// A fresh access token.
    pub access: Secret,
    /// Its expiry (wire instant).
    pub access_expires_at: String,
}

/// An access token for `account_id` in `family_id`.
pub fn access_token(
    state: &AppState,
    account_id: Uuid,
    family_id: Uuid,
    role: &str,
) -> Result<(Secret, String), ApiError> {
    let issued = access::issue(
        state.id().signing_keys()?,
        &account_id.to_string(),
        &family_id.to_string(),
        Audience::Launcher,
        role,
        now_secs(),
    );
    Ok((issued.token, views::at_secs(issued.expires_at)))
}

fn mint(kind: Kind) -> Result<opaque::Minted, ApiError> {
    opaque::mint(kind).map_err(|_| ApiError::unavailable())
}

/// Binds `device` to `account_id` (under the account lock) and starts a launcher family on it.
pub async fn issue_launcher_in(
    conn: &mut PgConnection,
    state: &AppState,
    account_id: Uuid,
    role: &str,
    device: &NewDevice,
) -> Result<LauncherTokens, ApiError> {
    accounts::lock_account_in(conn, account_id).await?;
    let binding = accounts::bind_device_in(conn, account_id, device).await?;
    let refresh = mint(Kind::RefreshToken)?;
    let issued = tokens::start_launcher_family_in(
        conn,
        account_id,
        binding.binding_id,
        &binding.platform,
        &refresh.lookup,
    )
    .await?;
    let (access, access_expires_at) = access_token(state, account_id, issued.family_id, role)?;
    Ok(LauncherTokens {
        account_id,
        refresh: refresh.token,
        issued,
        access,
        access_expires_at,
    })
}

/// Re-issues the family of a replayed launcher sign-in (CR-L4) for the request `(scope, idem)`:
/// the family its record names, which the client never received, is revoked (`replayed`), a new
/// one starts on the same device and the record is re-pointed at it. `None` when that family is no
/// longer live (signed out, reused, banned, deleted, or the token is unknown): not a replay, the
/// route answers its refusal. The caller holds [`limits::lock_key_in`].
pub async fn reissue_launcher_in(
    conn: &mut PgConnection,
    state: &AppState,
    scope: &str,
    idem: &Idempotency,
    first_token: Uuid,
) -> Result<Option<LauncherTokens>, ApiError> {
    let refresh = mint(Kind::RefreshToken)?;
    let Some(reissued) =
        tokens::replay_family_in(conn, scope, idem, first_token, &refresh.lookup).await?
    else {
        return Ok(None);
    };
    if reissued.account.banned {
        return Err(crate::errors::banned(
            reissued.account.ban_review.as_deref(),
        ));
    }
    let account_id = reissued.account.account_id;
    let (access, access_expires_at) = access_token(
        state,
        account_id,
        reissued.issued.family_id,
        &reissued.account.role,
    )?;
    Ok(Some(LauncherTokens {
        account_id,
        refresh: refresh.token,
        issued: reissued.issued,
        access,
        access_expires_at,
    }))
}

/// A portal session just opened.
#[derive(Debug)]
pub struct PortalSessionTokens {
    /// The account.
    pub account_id: Uuid,
    /// `fi_ps_…` (shown once, to the BFF).
    pub token: Secret,
    /// Its row.
    pub issued: IssuedToken,
}

/// Opens a portal session for `account_id`.
pub async fn issue_portal_in(
    conn: &mut PgConnection,
    account_id: Uuid,
) -> Result<PortalSessionTokens, ApiError> {
    let token = mint(Kind::PortalSession)?;
    let issued = tokens::start_portal_session_in(conn, account_id, &token.lookup).await?;
    Ok(PortalSessionTokens {
        account_id,
        token: token.token,
        issued,
    })
}

/// Re-issues a replayed portal sign-in's session (the one its record names is revoked, the record
/// re-pointed at the new one); `None` when that session is no longer live (not a replay). The
/// caller holds [`limits::lock_key_in`].
pub async fn reissue_portal_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
    first_token: Uuid,
) -> Result<Option<PortalSessionTokens>, ApiError> {
    let token = mint(Kind::PortalSession)?;
    Ok(
        tokens::replay_family_in(conn, scope, idem, first_token, &token.lookup)
            .await?
            .map(|r| PortalSessionTokens {
                account_id: r.account.account_id,
                token: token.token,
                issued: r.issued,
            }),
    )
}

/// Re-issues the identity ticket of a replayed `needs_account` (CR-L4): the ticket its record
/// names is spent, a new one minted with its claims and the record re-pointed at it. `None` when
/// that ticket was redeemed (the client had it) or expired: not a replay. The caller holds
/// [`limits::lock_key_in`].
pub async fn reissue_ticket_in(
    conn: &mut PgConnection,
    scope: &str,
    idem: &Idempotency,
    ticket_id: Uuid,
) -> Result<Option<(Secret, identity::Minted)>, ApiError> {
    let ticket = mint(Kind::IdentityTicket)?;
    Ok(
        identity::replay_ticket_in(conn, scope, idem, ticket_id, &ticket.lookup)
            .await?
            .map(|m| (ticket.token, m)),
    )
}

/// The e-mail a `needs_account` answer shows: the claim's, when verified.
pub fn shown_email(claim: &IdentityClaim) -> Option<String> {
    claim.email_lower.clone().filter(|_| claim.email_verified)
}

/// The `response` stored with a sign-in's idempotency record: which outcome, never a secret.
pub fn outcome(kind: &str) -> serde_json::Value {
    serde_json::json!({ "outcome": kind })
}

/// The outcome a record names.
pub fn recorded_outcome(response: &Option<serde_json::Value>) -> Option<&str> {
    response.as_ref()?.get("outcome")?.as_str()
}
