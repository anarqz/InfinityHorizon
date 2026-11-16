//! fi-core (platform v1, ADR 0013; CORE_API.md): the public, account, social, admin and MCP
//! surface. It mounts fi-api's identity routes of `/launcher/v1` (not sessions, not saves), the
//! web's `/portal/v1` routes and `/internal/v1`, and adds `/core/v1`, the web's cookie session
//! behind Turnstile, the MCP skeleton, in-app rate limits and the edge lock (ADR 0903).
//!
//! | Module | What |
//! | --- | --- |
//! | [`config`] | `FI_WEB_ORIGIN`, Turnstile, `FI_EDGE_SECRET`, MCP, probes, support links |
//! | [`guard`] | client address, token buckets per route class, no game ticket on core |
//! | [`web`] | CORS, CSRF, the `__Host-fi_session` cookie, sign-in, sign-up, sign-out |
//! | [`turnstile`] | `X-Fi-Challenge` spend-once and siteverify (test secrets offline) |
//! | [`session`] | the platform session: launcher bearer or web cookie, roles |
//! | [`status`] | `/core/v1/status`, `/core/v1/notices` |
//! | [`me`] | `/core/v1/me/*` |
//! | [`social`] | `/core/v1/social/*` |
//! | [`friends`] | `/core/v1/me/friends*` and the launcher's `/launcher/v1/friends*` (with presence) |
//! | [`invites`] | the launcher's `/launcher/v1/invites*`: pending invites and requests to join |
//! | [`reports`] | the launcher's `/launcher/v1/reports`: a report from the game (OD-63) |
//! | [`admin`] | `/core/v1/admin/*` |
//! | [`discord`] | the Discord feed, link and announce switch (CR-S63) |
//! | [`mcp`] | `/mcp` and the OAuth discovery documents |

pub mod admin;
pub mod config;
pub mod discord;
pub mod friends;
pub mod guard;
pub mod invites;
pub mod mcp;
pub mod me;
pub mod reports;
pub mod session;
pub mod social;
pub mod status;
pub mod turnstile;
pub mod voice;
pub mod web;

#[cfg(test)]
mod e2e;

use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use fi_protocol::routes::core as r;
use fi_runtime::ratelimit::TokenBuckets;
use fi_runtime::{ServiceInfo, http};
use fi_surfaces::config::PlatformConfig;
use fi_surfaces::errors::ApiError;
use fi_surfaces::state::{AppState, Platform};
use std::sync::Arc;
use uuid::Uuid;

/// Largest request body read by any route of core (the identity routes refuse more than 64 KiB
/// with `REQUEST.TOO_LARGE`; this bound only caps what is buffered before that check).
pub const MAX_BODY: usize = 128 * 1024;

/// What core's handlers share beyond fi-api's [`AppState`]: configuration and the per-instance,
/// derived state (token buckets, the probe cache), whose loss costs nothing but a fresh start.
pub struct Core {
    /// Settings.
    pub config: config::CoreConfig,
    /// In-app token buckets.
    pub buckets: TokenBuckets,
    /// Turnstile.
    pub turnstile: turnstile::Verifier,
    /// Status probes.
    pub probes: status::Probes,
}

impl Core {
    /// Core with `config`.
    pub fn new(config: config::CoreConfig) -> Self {
        Self {
            buckets: TokenBuckets::default(),
            turnstile: turnstile::Verifier::new(config.turnstile.clone()),
            probes: status::Probes::new(config.probes.clone()),
            config,
        }
    }
}

/// `^[a-z][a-z0-9]{1,15}$`, the `game.code` check.
pub fn valid_game(code: &str) -> bool {
    let b = code.as_bytes();
    (2..=16).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The id cursor of a page (`next` of the previous page): a UUID.
pub(crate) fn cursor_id(cursor: Option<&str>) -> Result<Option<Uuid>, ApiError> {
    cursor
        .map(|c| {
            Uuid::parse_str(c)
                .map_err(|_| ApiError::invalid("cursor", "the next value of a previous page"))
        })
        .transpose()
}

/// `next` of a page: the last row's id when the page is full.
pub(crate) fn next_id<T>(rows: &[T], page: i64, id: impl Fn(&T) -> Uuid) -> Option<String> {
    (rows.len() as i64 == page)
        .then(|| rows.last().map(|r| id(r).to_string()))
        .flatten()
}

fn core_routes() -> Router<AppState> {
    Router::new()
        .route(r::STATUS, get(status::status))
        .route(r::SUMMARY, get(status::summary))
        .route(r::NOTICES, get(status::board))
        .route(r::ME_NOTICES, get(me::notices))
        .route(r::ME_STRIKES, get(me::strikes))
        .route(r::ME_SUPPORT, get(me::support))
        .route(r::ME_DISCORD, get(discord::settings))
        .route(
            r::ME_DISCORD_LINK,
            post(discord::new_code).delete(discord::unlink),
        )
        .route(
            r::ME_PROFILE_ANNOUNCE,
            axum::routing::put(discord::set_announce),
        )
        .route(r::FEED, get(discord::feed))
        .route(r::DISCORD_LINK_PREVIEW, post(discord::link_preview))
        .route(r::DISCORD_LINKS, axum::routing::put(discord::link))
        .route(r::DISCORD_USER, axum::routing::put(discord::refresh_user))
        .route(r::SOCIAL_PLAYERS, get(social::players))
        .route(r::SOCIAL_PLAYER, get(social::player))
        .route(
            r::SOCIAL_COMMENTS,
            get(social::comments).post(social::post_comment),
        )
        .route(r::SOCIAL_COMMENT, delete(social::delete_comment))
        .route(r::SOCIAL_REPORTS, post(social::post_report))
        .route(
            r::ADMIN_MAINTENANCE,
            get(admin::get_maintenance).put(admin::put_maintenance),
        )
        .route(
            r::ADMIN_CAPACITY,
            get(admin::get_capacity).put(admin::put_capacity),
        )
        .route(
            r::ADMIN_NOTICES,
            get(admin::list_notices).post(admin::create_notice),
        )
        .route(r::ADMIN_NOTICE, delete(admin::delete_notice))
        .route(r::ADMIN_ACCOUNTS, get(admin::lookup))
        .route(r::ADMIN_ACCOUNT, get(admin::account))
        .route(r::ADMIN_STRIKES, post(admin::create_strike))
        .route(r::ADMIN_STRIKE_REVIEW, post(admin::review_strike))
        .route(r::ADMIN_BANS, post(admin::ban))
        .route(r::ADMIN_BAN, delete(admin::lift_ban))
        .route(r::ADMIN_KICKS, post(admin::kick))
        .route(r::ADMIN_VOICE_BANS, post(admin::voice_ban))
        .route(r::ADMIN_VOICE_BAN, delete(admin::lift_voice_ban))
        .route(r::ADMIN_CHEAT_FLAG, delete(admin::lift_cheat_flag))
        .route(r::ADMIN_REPORTS, get(admin::reports))
        .route(r::ADMIN_REPORT, post(admin::decide_report))
        .route(r::ADMIN_ROLE, axum::routing::put(admin::set_role))
        .route(r::ADMIN_ERASE, post(admin::erase))
        .route(r::ADMIN_AUDIT, get(admin::audit))
        .route(r::ADMIN_ASSESSMENTS, get(admin::assessments))
        .route(
            r::ADMIN_ACCOUNT_LIVERIES,
            get(admin::account_liveries).post(admin::bulk_liveries),
        )
        .route(
            r::ADMIN_LIVERY_UPLOADS,
            axum::routing::put(admin::set_livery_uploads),
        )
        .route(r::ADMIN_SEED_REVOKE, post(admin::seed_revoke))
        .route(
            r::ADMIN_LIVERY,
            axum::routing::put(admin::set_livery).delete(admin::delete_livery),
        )
        .route(
            r::MCP,
            get(mcp::get)
                .post(mcp::post)
                .layer(axum::extract::DefaultBodyLimit::max(mcp::MAX_BODY)),
        )
        .route(r::MCP_RESOURCE_METADATA, get(mcp::resource_metadata))
        .route(r::MCP_AUTH_METADATA, get(mcp::authorization_metadata))
}

/// Health (readiness = Postgres and this build's schema), meta, fi-api's identity, web-portal and
/// internal routers, and core's own routes, behind the guards of [`guard`] and [`web`].
pub fn router(
    info: ServiceInfo,
    state: AppState,
    platform: PlatformConfig,
    core: Arc<Core>,
) -> Router {
    let control_pool = state.pool.clone();
    let pool = state.pool.clone();
    let ready = move || {
        let pool = pool.clone();
        async move {
            fi_db::ping(&pool)
                .await
                .map_err(|_| "database unreachable".to_owned())?;
            fi_db::migrate::check_applied(&pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    };
    let edge = core.config.edge.clone();
    let routes = layered(
        http::health_router(ready)
            .merge(http::meta_router(info))
            .merge(api(state)),
        platform,
        core.clone(),
    );
    // Browser callers must receive readable closure/status responses and valid preflights.
    let routes = fi_surfaces::online_control::protect(routes, control_pool)
        .layer(axum::middleware::from_fn_with_state(core, web::cors));
    fi_surfaces::extract::origin_locked(routes, edge)
}

fn api(state: AppState) -> Router {
    Router::new()
        .merge(fi_surfaces::launcher::identity_router(&state))
        .merge(fi_surfaces::portal::web_router())
        .merge(fi_surfaces::internal::router())
        .merge(web::router())
        .merge(core_routes())
        .merge(friends::router())
        .merge(invites::router())
        .merge(reports::router())
        .merge(voice::router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            session::require_role,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            guard::ticket_guard,
        ))
        .with_state(state)
}

fn layered(routes: Router, platform: PlatformConfig, core: Arc<Core>) -> Router {
    let platform: Platform = Arc::new(platform);
    routes
        // No body core accepts is larger (fi-api's identity bodies: 64 KiB; review S-2).
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .layer(Extension::<Platform>(platform))
        .layer(Extension(core.clone()))
        .layer(axum::middleware::from_fn_with_state(
            core.clone(),
            web::csrf,
        ))
        .layer(axum::middleware::from_fn_with_state(
            core,
            guard::rate_limit,
        ))
}

/// `routes` behind core's CORS (as [`router`] wraps health, meta and the online status), for the
/// shell of a process that mounts fi-core's surface.
pub fn browser(routes: Router, core: Arc<Core>) -> Router {
    routes.layer(axum::middleware::from_fn_with_state(core, web::cors))
}

/// Core's routes with core's guards (body limit, CSRF, rate limits, ticket guard, roles) behind
/// `online`, and CORS outside it as in [`router`], for a process that mounts several surfaces
/// (ADR 0023 §1, `fi-infinity`): no health, meta or origin lock.
pub fn surface(
    state: AppState,
    platform: PlatformConfig,
    core: Arc<Core>,
    online: &fi_surfaces::online_control::OnlineGuard,
) -> Router {
    online
        .guard(layered(api(state), platform, core.clone()))
        .layer(axum::middleware::from_fn_with_state(core, web::cors))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_codes_match_the_column_check() {
        for ok in ["fh2", "fh3", "fm7", "ab"] {
            assert!(valid_game(ok), "{ok}");
        }
        for bad in ["", "f", "FH2", "2fh", "fh-2", "abcdefghijklmnopq"] {
            assert!(!valid_game(bad), "{bad}");
        }
        assert!(cursor_id(Some("x")).is_err());
        assert_eq!(cursor_id(None).unwrap(), None);
        let ids = [Uuid::now_v7(), Uuid::now_v7()];
        assert_eq!(next_id(&ids, 2, |u| *u), Some(ids[1].to_string()));
        assert_eq!(next_id(&ids, 3, |u| *u), None);
    }
}
