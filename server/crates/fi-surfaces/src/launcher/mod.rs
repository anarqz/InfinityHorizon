//! `/launcher/v1` (ACCOUNTS.md §5): the identity routes of SER-1.1 ([`identity_router`], served by
//! `fi-core` in platform v1), plus the play sessions ([`sessions`], the gate's) and saves
//! ([`saves`], the saves service's) of SAVES_API.md, built for the local environment v0.

pub mod accounts;
pub mod auth;
pub mod local;
pub mod local_mail;
pub mod saves;
pub mod sessions;

use crate::state::AppState;
use axum::Router;
use axum::routing::{delete, get, post};
use fi_protocol::routes::launcher as r;

/// The launcher surface: identity, play sessions and saves (the local environment v0's `api`).
pub fn router(state: &AppState) -> Router<AppState> {
    identity_router(state)
        .merge(sessions::router())
        .merge(saves::router())
}

/// The identity and account routes only (`info`, sign-in exchange, tokens, accounts, `/me`,
/// profiles, deletion): what `fi-core` serves of `/launcher/v1`. Play sessions are the gate's,
/// the save line the saves service's (CORE_API.md, ADR 0013). The `local` sign-in routes
/// ([`local`]) exist only when `state` runs `FI_AUTH_BACKEND=local` (404 otherwise).
pub fn identity_router(state: &AppState) -> Router<AppState> {
    let routes = Router::new();
    let routes = if state.identity.as_deref().is_some_and(|i| i.is_local()) {
        routes
            .route(r::AUTH_SIGN_UP, post(local::sign_up))
            .route(r::AUTH_SIGN_IN, post(local::sign_in))
            .route(r::AUTH_PASSWORD, post(local::change_password))
            .route(r::AUTH_CONFIRMATION, post(local_mail::resend_confirmation))
            .route(r::AUTH_CONFIRMATION_CODE, post(local_mail::confirm_code))
            .route(
                r::AUTH_PASSWORD_RESET,
                post(local_mail::request_password_reset),
            )
            .route(
                r::AUTH_EMAIL_CONFIRM,
                get(local_mail::confirm_page).post(local_mail::confirm_submit),
            )
            .route(
                r::AUTH_EMAIL_PASSWORD,
                get(local_mail::password_page).post(local_mail::password_submit),
            )
    } else {
        routes
    };
    routes
        .route(r::INFO, get(auth::info))
        .route(r::AUTH_TOKEN, post(auth::auth_token))
        .route(r::TOKEN, post(auth::token))
        .route(r::SIGN_OUT, post(auth::sign_out))
        .route(r::ACCOUNTS, post(accounts::create))
        .route(r::LEGACY_LINK, post(accounts::legacy_link))
        .route(r::ME, get(accounts::me))
        .route(r::PROFILES, post(accounts::add_profile))
        .route(r::PROFILE, delete(accounts::delete_profile))
        .route(r::PROFILE_MODE, post(accounts::change_mode))
        .route(r::PROFILE_NAME, post(accounts::rename_profile))
        .route(r::PROFILE_WEBSITE, post(accounts::set_website))
        .route(r::PROFILE_PRESENCE, get(accounts::presence))
        .route(r::PLAYER, delete(accounts::delete_player))
        .route(r::PLAYER_CAREERS, post(accounts::add_career))
        // CR-S77: the public preview images the presence names, on the same (core) origin.
        .merge(crate::media::router())
        .route(
            r::ME_DELETION,
            post(accounts::request_deletion).delete(accounts::cancel_deletion),
        )
}
