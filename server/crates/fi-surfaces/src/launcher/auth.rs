//! `/launcher/v1/info`, the sign-in exchange (`/auth/token`), refresh rotation
//! (`/token`) and sign-out (ACCOUNTS.md §4.1, §5; CR-L4 replay).

use crate::errors::ApiError;
use crate::extract::{Body, ClientNetwork, IdempotencyKey, LauncherAuth};
use crate::signin::{self, LauncherTokens};
use crate::state::AppState;
use crate::views;
use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use fi_auth::opaque::{self, Kind};
use fi_db::accounts;
use fi_db::identity::{self, Client, IdentityClaim};
use fi_db::limits::{self, Idempotency, Recorded};
use fi_db::tokens::{self, Rotation};
use fi_protocol::ErrorCode;
use fi_protocol::accounts::{GameInfo, Refresh, ServerInfo, SignInResult, TokenGrant, TokenPair};
use sqlx::{PgConnection, Postgres, Transaction};

const AUTH_TOKEN: &str = "POST /launcher/v1/auth/token";
const TOKEN: &str = "POST /launcher/v1/token";

/// *public* `GET /launcher/v1/info`: the deployment's settings plus the games and their switches
/// from the `game` table (one read; a game's floors default to the deployment's). Edge-cached
/// 60 s (CORE_API.md §2.1).
pub async fn info(
    State(state): State<AppState>,
) -> Result<([(header::HeaderName, &'static str); 1], Json<ServerInfo>), ApiError> {
    let mut info = state.id().info.clone();
    let mut conn = state.pool.acquire().await?;
    info.registration_enabled = signin::registration_open(&state, &mut conn).await?;
    info.games = fi_db::notices::listed_games_in(&mut conn)
        .await?
        .into_iter()
        .map(|g| {
            Ok(GameInfo {
                accepting: g.accepting && g.maintenance_until.is_none(),
                maintenance_until: g.maintenance_until.map(views::at),
                min_hook: g.min_hook.unwrap_or_else(|| info.min_hook.clone()),
                min_launcher: fi_domain::version::effective_min(
                    &info.min_launcher,
                    g.min_launcher.as_deref(),
                )
                .map_err(|_| ApiError::unavailable())?
                .to_owned(),
                id: g.code,
                title: g.title,
            })
        })
        .collect::<Result<_, ApiError>>()?;
    Ok((
        [(header::CACHE_CONTROL, "public, max-age=60, s-maxage=60")],
        Json(info),
    ))
}

fn signed_in(t: LauncherTokens) -> SignInResult {
    SignInResult::SignedIn {
        account_id: t.account_id.to_string(),
        refresh_token: t.refresh.into_inner(),
        access_token: t.access.into_inner(),
        access_expires_at: t.access_expires_at,
        profile: None,
    }
}

fn key_reused() -> ApiError {
    ApiError::new(
        ErrorCode::RequestIdempotencyKeyReused,
        "this Idempotency-Key was used for another request",
    )
}

/// The CR-L4 replay of an exchange whose answer was lost: same key, same body. A `signed_in`
/// re-issues the family the record names (revoked, a new one started, the record re-pointed) while
/// that family is live; a `needs_account` re-issues its ticket while unredeemed. `None`: not a
/// replay (no record, or what it names is gone: signed out, revoked, redeemed), and the caller
/// answers its refusal. `claim` is the identity verified before (the `id_token` grant): its
/// e-mail is shown again. The caller holds
/// [`limits::lock_key_in`].
async fn replay(
    tx: &mut PgConnection,
    state: &AppState,
    scope: &str,
    idem: &Idempotency,
    claim: Option<&IdentityClaim>,
) -> Result<Option<SignInResult>, ApiError> {
    let record = match limits::recorded_in(tx, scope, idem).await? {
        Recorded::None => return Ok(None),
        Recorded::Different => return Err(key_reused()),
        Recorded::Same(record) => record,
    };
    let Some(resource) = record.resource_id else {
        return Ok(None);
    };
    if signin::recorded_outcome(&record.response) == Some("needs_account") {
        signin::require_registration(state, tx).await?;
    }
    match signin::recorded_outcome(&record.response) {
        Some("signed_in") => Ok(
            signin::reissue_launcher_in(tx, state, scope, idem, resource)
                .await?
                .map(signed_in),
        ),
        Some("needs_account") => Ok(signin::reissue_ticket_in(tx, scope, idem, resource)
            .await?
            .map(|(ticket, m)| SignInResult::NeedsAccount {
                identity_ticket: ticket.into_inner(),
                identity_ticket_expires_at: views::at(m.expires_at),
                email: claim.and_then(signin::shown_email),
            })),
        _ => Ok(None),
    }
}

/// The exchange's outcome for a verified identity: `signed_in` (device bound, family started) or
/// `needs_account` (an identity ticket).
async fn exchange(
    tx: &mut PgConnection,
    state: &AppState,
    claim: &IdentityClaim,
    device: &accounts::NewDevice,
    scope: &str,
    idem: &Idempotency,
) -> Result<SignInResult, ApiError> {
    if !matches!(limits::recorded_in(tx, scope, idem).await?, Recorded::None) {
        return Err(key_reused());
    }
    match accounts::find_by_uid_in(tx, claim).await? {
        Some(account) => {
            if account.banned {
                return Err(crate::errors::banned(account.ban_review.as_deref()));
            }
            let t = signin::issue_launcher_in(tx, state, account.account_id, &account.role, device)
                .await?;
            limits::record_with_response_in(
                tx,
                scope,
                idem,
                200,
                Some(t.issued.token_id),
                Some(&signin::outcome("signed_in")),
            )
            .await?;
            Ok(signed_in(t))
        }
        None => {
            signin::require_registration(state, tx).await?;
            let ticket = opaque::mint(Kind::IdentityTicket).map_err(|_| ApiError::unavailable())?;
            let minted =
                identity::mint_ticket_in(tx, &ticket.lookup, claim, Client::Launcher).await?;
            limits::record_with_response_in(
                tx,
                scope,
                idem,
                200,
                Some(minted.id),
                Some(&signin::outcome("needs_account")),
            )
            .await?;
            Ok(SignInResult::NeedsAccount {
                identity_ticket: ticket.token.into_inner(),
                identity_ticket_expires_at: views::at(minted.expires_at),
                email: signin::shown_email(claim),
            })
        }
    }
}

async fn commit(tx: Transaction<'_, Postgres>) -> Result<(), ApiError> {
    tx.commit().await.map_err(ApiError::from)
}

/// Public POST /launcher/v1/auth/token: native ID-token exchange only (OD-74).
pub async fn auth_token(
    State(state): State<AppState>,
    key: IdempotencyKey,
    net: ClientNetwork,
    Body(grant): Body<TokenGrant>,
) -> Result<Json<SignInResult>, ApiError> {
    let TokenGrant::IdToken {
        id_token,
        device: body_device,
    } = &grant;
    let device = signin::device(&state, body_device)?;
    let idem = key.of(AUTH_TOKEN, &grant);
    let scope = format!("device:{}", device.device_id);
    // Count attempts before verification; verify outside the transaction (key-set I/O).
    // Never acquire a second pooled connection while this request holds one.
    signin::limit(
        &state,
        &limits::SIGNIN_NET,
        &net.0,
        ErrorCode::AuthRateLimited,
    )
    .await?;
    let claim = signin::verify_id_token(&state, id_token).await?;
    let mut tx = state.pool.begin().await?;
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    count_identity(&mut tx, &state, &claim).await?;
    super::local::require_current_in(&mut tx, &state, &claim).await?;
    if let Some(result) = replay(&mut tx, &state, &scope, &idem, Some(&claim)).await? {
        commit(tx).await?;
        return Ok(Json(result));
    }
    let result = exchange(&mut tx, &state, &claim, &device, &scope, &idem).await?;
    commit(tx).await?;
    Ok(Json(result))
}

/// The per-identity limit counts committed native exchanges, replays included.
async fn count_identity(
    tx: &mut PgConnection,
    state: &AppState,
    claim: &IdentityClaim,
) -> Result<(), ApiError> {
    signin::limit_in(
        tx,
        state,
        &limits::SIGNIN_UID,
        &format!("uid:{}", claim.idp_uid),
        ErrorCode::AuthRateLimited,
    )
    .await
}

/// *public* `POST /launcher/v1/token`: rotate the refresh token (reuse detection, CR-L4 replay).
pub async fn token(
    State(state): State<AppState>,
    key: IdempotencyKey,
    Body(req): Body<Refresh>,
) -> Result<Json<TokenPair>, ApiError> {
    let invalid = || ApiError::new(ErrorCode::AuthRefreshInvalid, "refresh token refused");
    let presented = opaque::lookup(Kind::RefreshToken, &req.refresh_token).ok_or_else(invalid)?;
    let idem = key.of(TOKEN, &req);
    let successor = opaque::mint(Kind::RefreshToken).map_err(|_| ApiError::unavailable())?;
    let mut tx = state.pool.begin().await?;
    let rotation = tokens::rotate_in(&mut tx, &presented, &successor.lookup, &idem).await?;
    // Committed whatever the outcome: a detected reuse must keep the family revoked.
    commit(tx).await?;
    match rotation {
        Rotation::Rotated {
            account_id,
            family_id,
            role,
            ..
        } => {
            let (access, access_expires_at) =
                signin::access_token(&state, account_id, family_id, &role)?;
            Ok(Json(TokenPair {
                access_token: access.into_inner(),
                access_expires_at,
                refresh_token: successor.token.into_inner(),
            }))
        }
        Rotation::Invalid => Err(invalid()),
        Rotation::Reused => Err(ApiError::new(
            ErrorCode::AuthRefreshReused,
            "refresh token reused: the family is revoked",
        )),
        Rotation::Banned { review } => Err(crate::errors::banned(review.as_deref())),
        Rotation::KeyReused => Err(key_reused()),
    }
}

/// `POST /launcher/v1/sign-out`: revokes this family (unbinds nothing).
pub async fn sign_out(
    State(state): State<AppState>,
    auth: LauncherAuth,
) -> Result<StatusCode, ApiError> {
    let mut conn = state.pool.acquire().await?;
    tokens::revoke_family_in(&mut conn, auth.family_id, "logout").await?;
    Ok(StatusCode::NO_CONTENT)
}
