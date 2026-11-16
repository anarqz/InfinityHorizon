//! The request guards of core, outermost first: the edge lock (`fi_runtime::http::edge_guard`),
//! [`client_context`] (who is calling, for limits and Turnstile; fi-api's, as on every
//! deployable), CORS (`crate::web::cors`),
//! [`rate_limit`] (the in-app token buckets per route class, ADR 0903 §3), CSRF
//! (`crate::web::csrf`) and [`ticket_guard`] (no game access ticket opens core, ADR 0014 §2).

use crate::Core;
use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use fi_auth::access::{AccessError, Audience};
use fi_protocol::ErrorCode;
use fi_runtime::http::is_health;
use fi_runtime::ratelimit::{Decision, Rate};
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{ClientNetwork, bearer};
use fi_surfaces::state::{AppState, now_secs};
use std::sync::Arc;
use uuid::Uuid;

/// The caller's address and network, shared with the other deployables (fi-api).
pub use fi_surfaces::extract::{ClientIp, client_context};

/// The bucket class of a request, `None` when the in-app limits do not apply: health probes,
/// preflights, the session-only reads of `/core/v1/me`, the staff routes of `/core/v1/admin`
/// (role-gated; their writes count per account), and `/launcher/v1` beyond `info` (per account in
/// `LauncherAuth`, and exact per route in Postgres, ACCOUNTS.md §9).
pub fn class(
    rates: &crate::config::Rates,
    method: &Method,
    path: &str,
) -> Option<(&'static str, Rate)> {
    if is_health(path) || method == Method::OPTIONS {
        return None;
    }
    if path.starts_with("/core/v1/me/") || path.starts_with("/core/v1/admin/") {
        return None;
    }
    // The Discord bot's routes (CR-S63): their own buckets, so polling never eats public reads
    // and link pages (driven by the public) never starve the feed.
    if path == fi_protocol::routes::core::FEED {
        return Some(("bot.feed", rates.bot));
    }
    if path.starts_with("/core/v1/discord/") {
        return Some(("bot.link", rates.bot));
    }
    let read = method == Method::GET || method == Method::HEAD;
    // CR-S77 (ACCOUNTS.md §5a): the public preview images, their own bucket per network.
    if read && path.starts_with("/media/v1/") {
        return Some(("media", rates.public_read));
    }
    let public_surface = path.starts_with("/core/v1/") || path.starts_with("/portal/v1/");
    if read
        && (public_surface
            || path == fi_protocol::routes::launcher::INFO
            || path.starts_with("/.well-known/")
            || path == "/meta")
    {
        return Some(("read", rates.public_read));
    }
    if path == fi_protocol::routes::core::MCP {
        return Some(("mcp", rates.public_read));
    }
    if !read && public_surface {
        return Some(("write", rates.public_write));
    }
    None
}

/// 429 `REQUEST.RATE_LIMITED` with `Retry-After` when the request's bucket is empty.
pub async fn rate_limit(State(core): State<Arc<Core>>, request: Request, next: Next) -> Response {
    let Some((name, rate)) = class(&core.config.rates, request.method(), request.uri().path())
    else {
        return next.run(request).await;
    };
    let subject = request
        .extensions()
        .get::<ClientNetwork>()
        .map(|n| n.0.clone())
        .unwrap_or_else(|| "unknown".into());
    match core.buckets.take(&format!("{name}:{subject}"), rate) {
        Decision::Allowed => next.run(request).await,
        Decision::Limited { retry_after_secs } => {
            tracing::info!(class = name, "rate limited");
            limited(retry_after_secs).into_response()
        }
    }
}

fn limited(retry_after_secs: u64) -> ApiError {
    ApiError::new(
        ErrorCode::RequestRateLimited,
        "too many requests, retry later",
    )
    .retry_after(retry_after_secs)
}

impl Core {
    /// The per-account write bucket (30/min): every session write of `/core/v1` and the web.
    pub fn account_write(&self, account_id: Uuid) -> Result<(), ApiError> {
        self.take(
            &format!("account:{account_id}"),
            self.config.rates.account_write,
        )
    }

    /// A per-account read bucket (30/min each, CORE_API.md §4.1, §4.2): `what` names it
    /// (`friends`, `invites`), so the overlay's invite polling never eats the friend list's.
    /// 429 `REQUEST.RATE_LIMITED` with `Retry-After` past it.
    pub fn account_read(&self, what: &'static str, account_id: Uuid) -> Result<(), ApiError> {
        self.take(
            &format!("read.{what}:{account_id}"),
            self.config.rates.account_read,
        )
    }

    /// The per-account comment bucket (10/min, CORE_API.md §4).
    pub fn comment(&self, account_id: Uuid) -> Result<(), ApiError> {
        self.take(&format!("comment:{account_id}"), self.config.rates.comment)
    }

    fn take(&self, key: &str, rate: Rate) -> Result<(), ApiError> {
        match self.buckets.take(key, rate) {
            Decision::Allowed => Ok(()),
            Decision::Limited { retry_after_secs } => Err(limited(retry_after_secs)),
        }
    }
}

/// No game access ticket (or any bearer that is not a launcher access token) opens core: such a
/// bearer is 401 `AUTH.TOKEN_INVALID` on every route but `/health/*`, `/meta`, `/mcp` (its own
/// 401 with the resource metadata) and the discovery documents. An expired launcher token passes
/// here and gets `AUTH.TOKEN_EXPIRED` from the route's extractor, as before.
pub async fn ticket_guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if is_health(path)
        || path == "/meta"
        || path == fi_protocol::routes::core::MCP
        || path.starts_with("/.well-known/")
        || !request
            .headers()
            .contains_key(axum::http::header::AUTHORIZATION)
    {
        return next.run(request).await;
    }
    let ok = match bearer(request.headers()) {
        Some(t) if t.starts_with(fi_auth::opaque::Kind::PortalSession.prefix()) => true,
        Some(t) => match &state.identity {
            Some(id) => !matches!(
                id.access_keys.verify(t, Audience::Launcher, now_secs()),
                Err(AccessError::Invalid)
            ),
            None => false,
        },
        None => false,
    };
    if ok {
        next.run(request).await
    } else {
        ApiError::new(ErrorCode::AuthTokenInvalid, "invalid access token").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Rates;

    #[test]
    fn route_classes_follow_adr_0903() {
        let r = Rates::default();
        let c = |m: Method, p: &str| class(&r, &m, p).map(|(n, _)| n);
        assert_eq!(c(Method::GET, "/core/v1/status"), Some("read"));
        assert_eq!(c(Method::GET, "/core/v1/summary"), Some("read"));
        assert_eq!(c(Method::GET, "/core/v1/social/players"), Some("read"));
        assert_eq!(c(Method::GET, "/launcher/v1/info"), Some("read"));
        assert_eq!(c(Method::POST, "/core/v1/social/reports"), Some("write"));
        assert_eq!(c(Method::POST, "/portal/v1/sessions"), Some("write"));
        assert_eq!(
            c(Method::DELETE, "/portal/v1/sessions/current"),
            Some("write")
        );
        assert_eq!(c(Method::POST, "/mcp"), Some("mcp"));
        assert_eq!(c(Method::GET, "/core/v1/feed"), Some("bot.feed"));
        assert_eq!(
            c(
                Method::GET,
                "/media/v1/previews/00000000000000000000000000000000.png"
            ),
            Some("media")
        );
        assert_eq!(c(Method::PUT, "/core/v1/discord/links"), Some("bot.link"));
        assert_eq!(c(Method::GET, "/core/v1/me/discord"), None);
        for (m, p) in [
            (Method::GET, "/health/ready"),
            (Method::GET, "/health/live"),
            (Method::OPTIONS, "/portal/v1/sessions"),
            (Method::GET, "/core/v1/me/notices"),
            (Method::POST, "/core/v1/admin/bans"),
            (Method::POST, "/launcher/v1/auth/token"),
        ] {
            assert_eq!(c(m.clone(), p), None, "{m} {p}");
        }
    }
}
