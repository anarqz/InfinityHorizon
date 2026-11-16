//! The portal's identity routes (PORTAL_API.md §3): the Identity Platform exchange
//! (`POST /sessions`), account creation, `GET /me`, sign-out, health.
//!
//! Scopes of idempotency records: `identity:<HMAC of the uid>` (no account may exist yet). ID
//! tokens are verified before any transaction opens (ACCOUNTS.md §4.1: no network inside one, and
//! tomorrow's key-set fetch is a network call).

use crate::errors::ApiError;
use crate::extract::{Body, ClientNetwork, IdempotencyKey, PortalAuth};
use crate::signin::{self, PortalSessionTokens};
use crate::state::AppState;
use crate::views;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use fi_auth::opaque::{self, Kind};
use fi_db::accounts;
use fi_db::identity::{self, Client};
use fi_db::limits::{self, Recorded};
use fi_db::tokens;
use fi_protocol::ErrorCode;
use fi_protocol::portal::{
    PortalAccountCreate, PortalHealth, PortalSessionIssued, PortalSignIn, PortalSignInResult,
    Viewer,
};
use uuid::Uuid;

const SESSIONS: &str = "POST /portal/v1/sessions";
const ACCOUNTS: &str = "POST /portal/v1/accounts";

fn key_reused() -> ApiError {
    ApiError::new(
        ErrorCode::RequestIdempotencyKeyReused,
        "this Idempotency-Key was used for another request",
    )
}

async fn viewer(state: &AppState, account_id: Uuid) -> Result<Viewer, ApiError> {
    let me = accounts::me(&state.pool, account_id)
        .await?
        .ok_or_else(ApiError::unavailable)?;
    Ok(views::viewer(&me))
}

async fn issued(
    state: &AppState,
    session: PortalSessionTokens,
) -> Result<PortalSessionIssued, ApiError> {
    Ok(PortalSessionIssued {
        viewer: viewer(state, session.account_id).await?,
        token: session.token.into_inner(),
        expires_at: views::at(session.issued.expires_at),
    })
}

/// `GET /portal/v1/health`: 200 when the surface can serve (the database answers, the schema is
/// this build's).
pub async fn health(State(state): State<AppState>) -> Result<Json<PortalHealth>, ApiError> {
    fi_db::ping(&state.pool)
        .await
        .map_err(|_| ApiError::unavailable())?;
    fi_db::migrate::check_applied(&state.pool)
        .await
        .map_err(|_| ApiError::unavailable())?;
    Ok(Json(PortalHealth {
        status: "ok".into(),
    }))
}

/// `POST /portal/v1/sessions {idToken}` → 201 `SignInResult` (portal variant).
pub async fn sessions(
    State(state): State<AppState>,
    key: IdempotencyKey,
    net: ClientNetwork,
    Body(req): Body<PortalSignIn>,
) -> Result<(StatusCode, Json<PortalSignInResult>), ApiError> {
    let claim = signin::verify_id_token(&state, &req.id_token).await?;
    signin::exchange_limits(&state, &claim, &net).await?;
    let scope = state.id().pepper()?.identity_scope(&claim.idp_uid);
    let idem = key.of(SESSIONS, &req);
    let mut tx = state.pool.begin().await?;
    // Requests under one key serialize (CR-L4): a concurrent duplicate waits for the first to
    // commit and replays it.
    limits::lock_key_in(&mut tx, &scope, &idem).await?;
    crate::launcher::local::require_current_in(&mut tx, &state, &claim).await?;
    match limits::recorded_in(&mut tx, &scope, &idem).await? {
        // CR-L4: the session the record names is re-issued while it is live, the ticket while it
        // is unredeemed; once signed out, revoked or redeemed, this is a new sign-in under a used
        // key, not a replay.
        Recorded::Same(record) => {
            let resource = record.resource_id.ok_or_else(key_reused)?;
            match signin::recorded_outcome(&record.response) {
                Some("signed_in") => {
                    let session = signin::reissue_portal_in(&mut tx, &scope, &idem, resource)
                        .await?
                        .ok_or_else(key_reused)?;
                    tx.commit().await?;
                    let body = issued(&state, session).await?;
                    return Ok((
                        StatusCode::CREATED,
                        Json(PortalSignInResult::SignedIn {
                            token: body.token,
                            expires_at: body.expires_at,
                            viewer: Box::new(body.viewer),
                        }),
                    ));
                }
                Some("needs_account") => {
                    signin::require_registration(&state, &mut tx).await?;
                    let (ticket, m) = signin::reissue_ticket_in(&mut tx, &scope, &idem, resource)
                        .await?
                        .ok_or_else(key_reused)?;
                    tx.commit().await?;
                    return Ok((
                        StatusCode::CREATED,
                        Json(PortalSignInResult::NeedsAccount {
                            identity_ticket: ticket.into_inner(),
                            identity_ticket_expires_at: views::at(m.expires_at),
                            email: signin::shown_email(&claim),
                        }),
                    ));
                }
                _ => return Err(key_reused()),
            }
        }
        Recorded::Different => return Err(key_reused()),
        Recorded::None => {}
    }
    match accounts::find_by_uid_in(&mut tx, &claim).await? {
        Some(account) => {
            if account.banned {
                return Err(crate::errors::banned(account.ban_review.as_deref()));
            }
            let session = signin::issue_portal_in(&mut tx, account.account_id).await?;
            limits::record_with_response_in(
                &mut tx,
                &scope,
                &idem,
                201,
                Some(session.issued.token_id),
                Some(&signin::outcome("signed_in")),
            )
            .await?;
            tx.commit().await?;
            let body = issued(&state, session).await?;
            Ok((
                StatusCode::CREATED,
                Json(PortalSignInResult::SignedIn {
                    token: body.token,
                    expires_at: body.expires_at,
                    viewer: Box::new(body.viewer),
                }),
            ))
        }
        None => {
            signin::require_registration(&state, &mut tx).await?;
            let ticket = opaque::mint(Kind::IdentityTicket).map_err(|_| ApiError::unavailable())?;
            let minted =
                identity::mint_ticket_in(&mut tx, &ticket.lookup, &claim, Client::Portal).await?;
            limits::record_with_response_in(
                &mut tx,
                &scope,
                &idem,
                201,
                Some(minted.id),
                Some(&signin::outcome("needs_account")),
            )
            .await?;
            tx.commit().await?;
            Ok((
                StatusCode::CREATED,
                Json(PortalSignInResult::NeedsAccount {
                    identity_ticket: ticket.token.into_inner(),
                    identity_ticket_expires_at: views::at(minted.expires_at),
                    email: signin::shown_email(&claim),
                }),
            ))
        }
    }
}

fn valid_locale(locale: &str) -> bool {
    let mut parts = locale.split('-');
    let first = parts.next().unwrap_or_default();
    (2..=3).contains(&first.len())
        && first.bytes().all(|b| b.is_ascii_lowercase())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// `POST /portal/v1/accounts {identityTicket, locale?}` → 201 `{token, expiresAt, viewer}`: an
/// account without profiles (the launcher adds them).
pub async fn accounts(
    State(state): State<AppState>,
    key: IdempotencyKey,
    net: ClientNetwork,
    Body(req): Body<PortalAccountCreate>,
) -> Result<(StatusCode, Json<PortalSessionIssued>), ApiError> {
    signin::require_registration(&state, &mut *state.pool.acquire().await?).await?;
    if req.locale.as_deref().is_some_and(|l| !valid_locale(l)) {
        return Err(ApiError::invalid(
            "locale",
            "a BCP 47 tag such as en or pt-BR",
        ));
    }
    let invalid = || {
        ApiError::new(
            ErrorCode::AuthIdentityTicketInvalid,
            "identity ticket unknown, used or expired: sign in again",
        )
    };
    let ticket = opaque::lookup(Kind::IdentityTicket, &req.identity_ticket).ok_or_else(invalid)?;
    let idem = key.of(ACCOUNTS, &req);
    for attempt in 0..2 {
        let mut tx = state.pool.begin().await?;
        let uid = identity::ticket_uid_in(&mut tx, &ticket, Client::Portal)
            .await?
            .ok_or_else(invalid)?;
        let scope = state.id().pepper()?.identity_scope(&uid);
        limits::lock_key_in(&mut tx, &scope, &idem).await?;
        match limits::recorded_in(&mut tx, &scope, &idem).await? {
            // CR-L4: re-issued while the session the record names is live; once it is signed out
            // or revoked, the ticket's first use is all there was.
            Recorded::Same(record) => {
                let first = record.resource_id.ok_or_else(ApiError::unavailable)?;
                let session = signin::reissue_portal_in(&mut tx, &scope, &idem, first)
                    .await?
                    .ok_or_else(invalid)?;
                tx.commit().await?;
                return Ok((StatusCode::CREATED, Json(issued(&state, session).await?)));
            }
            Recorded::Different => return Err(key_reused()),
            Recorded::None => {}
        }
        signin::limit_in(
            &mut tx,
            &state,
            &limits::CREATE_NET,
            &net.0,
            ErrorCode::RequestRateLimited,
        )
        .await?;
        let Some(t) = identity::consume_ticket_in(&mut tx, &ticket, Client::Portal).await? else {
            if attempt == 0 {
                continue;
            }
            return Err(invalid());
        };
        let (account_id, _) =
            accounts::create_account_in(&mut tx, &t.claim, req.locale.as_deref(), None).await?;
        accounts::audit_in(&mut tx, account_id, "account.created").await?;
        let session = signin::issue_portal_in(&mut tx, account_id).await?;
        limits::record_with_response_in(
            &mut tx,
            &scope,
            &idem,
            201,
            Some(session.issued.token_id),
            Some(&signin::outcome("account_created")),
        )
        .await?;
        tx.commit().await?;
        return Ok((StatusCode::CREATED, Json(issued(&state, session).await?)));
    }
    Err(invalid())
}

/// `GET /portal/v1/me` → `Viewer`.
pub async fn me(State(state): State<AppState>, auth: PortalAuth) -> Result<Json<Viewer>, ApiError> {
    Ok(Json(viewer(&state, auth.account_id).await?))
}

/// `DELETE /portal/v1/sessions/current` → 204.
pub async fn sign_out(
    State(state): State<AppState>,
    auth: PortalAuth,
) -> Result<StatusCode, ApiError> {
    let mut conn = state.pool.acquire().await?;
    tokens::revoke_family_in(&mut conn, auth.family_id, "logout").await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::valid_locale;

    #[test]
    fn locales_match_the_column_check() {
        for ok in ["en", "pt-BR", "zh-Hant-TW", "ast"] {
            assert!(valid_locale(ok), "{ok}");
        }
        for bad in ["", "EN", "e", "pt_BR", "pt-", "english", "pt-B"] {
            assert!(!valid_locale(bad), "{bad}");
        }
    }
}
