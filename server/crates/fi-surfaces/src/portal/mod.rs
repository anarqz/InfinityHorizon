//! `/portal/v1` (PORTAL_API.md): the portal server's API. Every call carries
//! `X-Fi-Client-Assertion` ([`crate::extract::portal_assertion`]).
//!
//! The identity routes of PORTAL_API.md §3 the launcher's sign-in loop cannot work without
//! (sessions, the launcher's authorization page, device approvals, accounts, `/me`) are written by
//! the server lane in [`identity`] (SER-1.1); the portal lane builds its BFF client on them, and
//! adds the rest of the surface's handlers (D-12, Q17).

pub mod identity;

use crate::extract::portal_assertion;
use crate::state::AppState;
use axum::Router;
use axum::routing::{delete, get, post};
use fi_protocol::routes::portal as r;

/// The routes of the web's surface that need no Turnstile and no cookie handling of their own:
/// health, the launcher's authorization page, device approvals, `/me`. `fi-core` mounts them
/// without the assertion layer (withdrawn for the web, PORTAL_API.md §1 as amended) and adds its
/// own `sessions`, `accounts` and sign-out, which verify the challenge and set or clear the
/// `__Host-fi_session` cookie around [`identity::sessions`], [`identity::accounts`] and
/// [`identity::sign_out`].
pub fn web_router() -> Router<AppState> {
    Router::new()
        .route(r::HEALTH, get(identity::health))
        .route(r::ME, get(identity::me))
}

/// The portal surface (the deprecated BFF's: every call carries the assertion).
pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route(r::HEALTH, get(identity::health))
        .route(r::SESSIONS, post(identity::sessions))
        .route(r::SESSIONS_CURRENT, delete(identity::sign_out))
        .route(r::ACCOUNTS, post(identity::accounts))
        .route(r::ME, get(identity::me))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            portal_assertion,
        ))
}
