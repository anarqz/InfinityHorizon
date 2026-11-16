//! The web's session on core (ADR 0207 §3, PORTAL_API.md §1 as amended): the opaque portal session
//! token travels only in the `__Host-fi_session` cookie (HttpOnly, Secure, SameSite=Strict,
//! Path=/), set by `POST /portal/v1/sessions` and `POST /portal/v1/accounts` (both behind
//! Turnstile) and cleared by `DELETE /portal/v1/sessions/current`; CORS allows exactly the web
//! origin with credentials; every write that does not carry a launcher bearer needs
//! `Origin` = the web origin and `X-Fi-Client: web/<version>` (a custom header forces a
//! preflight, so a cross-site form cannot send it).

use crate::Core;
use crate::guard::ClientIp;
use crate::turnstile;
use axum::extract::{Extension, Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_CREDENTIALS, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
    ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE,
    ACCESS_CONTROL_REQUEST_METHOD, AUTHORIZATION, CACHE_CONTROL, ORIGIN, SET_COOKIE, VARY,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Json, Router};
use fi_protocol::ErrorCode;
use fi_protocol::core::{WebSession, WebSignInResult};
use fi_protocol::headers;
use fi_protocol::portal::{PortalAccountCreate, PortalSignIn, PortalSignInResult};
use fi_protocol::routes::portal as r;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, ClientNetwork, IdempotencyKey, PortalAuth, SESSION_COOKIE};
use fi_surfaces::portal::identity;
use fi_surfaces::state::AppState;
use std::sync::Arc;

/// Headers the web may send cross-origin.
pub const ALLOWED_HEADERS: &str =
    "content-type, idempotency-key, x-fi-client, x-fi-challenge, x-request-id";
/// Methods the web may use cross-origin.
pub const ALLOWED_METHODS: &str = "GET, POST, PUT, DELETE";

/// The web routes core owns: sign-in and sign-up (challenged, cookie set) and sign-out.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::SESSIONS, post(sessions))
        .route(r::ACCOUNTS, post(accounts))
        .route(r::SESSIONS_CURRENT, delete(sign_out))
}

fn is_write(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn origin_is_web(headers: &HeaderMap, web: &str) -> bool {
    headers
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| o == web)
}

/// CORS for exactly the web origin, with credentials. A preflight from it answers 204 with the
/// allowed methods and headers; a preflight from anywhere else answers 403 without CORS headers.
/// Every other answer names the web origin whatever the request's `Origin` (a browser on another
/// origin is refused all the same), so an edge-cached public answer carries the same CORS headers
/// for every client (review S-1). An answer without `Cache-Control` gets `no-store`: only the
/// public routes that set their own may be cached at the edge (review S-3).
pub async fn cors(State(core): State<Arc<Core>>, request: Request, next: Next) -> Response {
    if !core.config.web_enabled {
        let path = request.uri().path();
        if path == "/portal/v1"
            || path.starts_with("/portal/v1/")
            || (request.method() == Method::POST
                && matches!(path, "/core/v1/social/comments" | "/core/v1/social/reports"))
        {
            return ApiError::unavailable().into_response();
        }
        if request.method() == Method::OPTIONS {
            return (StatusCode::FORBIDDEN, [(CACHE_CONTROL, "no-store")]).into_response();
        }
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .entry(CACHE_CONTROL)
            .or_insert(HeaderValue::from_static("no-store"));
        return response;
    }
    let web = core.config.web_origin.as_str();
    let from_web = origin_is_web(request.headers(), web);
    let preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = if preflight {
        if from_web {
            let mut r = StatusCode::NO_CONTENT.into_response();
            let h = r.headers_mut();
            h.insert(
                ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static(ALLOWED_METHODS),
            );
            h.insert(
                ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static(ALLOWED_HEADERS),
            );
            h.insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
            r
        } else {
            StatusCode::FORBIDDEN.into_response()
        }
    } else {
        next.run(request).await
    };
    let h = response.headers_mut();
    h.append(VARY, HeaderValue::from_static("Origin"));
    if !h.contains_key(CACHE_CONTROL) {
        h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    if (from_web || !preflight)
        && let Ok(origin) = HeaderValue::from_str(web)
    {
        h.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.insert(
            ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        h.insert(
            ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("retry-after, x-request-id"),
        );
    }
    response
}

fn web_client(headers: &HeaderMap) -> bool {
    headers
        .get(headers::FI_CLIENT)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("web/"))
        .is_some_and(|v| (1..=32).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic()))
}

/// CSRF: a write to `/portal/v1` or `/core/v1` without `Authorization` (the cookie path) needs
/// `Origin` = the web origin and `X-Fi-Client: web/<version>`, else 403 `AUTH.FORBIDDEN`. The
/// launcher's bearer writes are not cookie-authenticated and pass, and so do the Discord bot's
/// (`/core/v1/discord/*`, CR-S63): they never accept a cookie, only `X-Fi-Feed-Key`.
pub async fn csrf(State(core): State<Arc<Core>>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let guarded = (path.starts_with("/portal/v1/") || path.starts_with("/core/v1/"))
        && !path.starts_with("/core/v1/discord/")
        && is_write(request.method())
        && !request.headers().contains_key(AUTHORIZATION);
    if guarded
        && !(origin_is_web(request.headers(), &core.config.web_origin)
            && web_client(request.headers()))
    {
        return ApiError::new(
            ErrorCode::AuthForbidden,
            "a browser write needs the web's Origin and X-Fi-Client",
        )
        .with_details(serde_json::json!({"reason": "csrf"}))
        .into_response();
    }
    next.run(request).await
}

/// `Set-Cookie` for a session token valid until `expires_at` (wire instant).
pub fn session_cookie(token: &str, expires_at: &str) -> HeaderValue {
    let now = fi_domain::UnixMs::now_system().millis() / 1_000;
    let until = fi_protocol::instant::parse_unix_micros(expires_at)
        .map(|m| m / 1_000_000)
        .unwrap_or(now);
    let max_age = (until - now).max(0);
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={max_age}; Secure; HttpOnly; SameSite=Strict"
    ))
    .unwrap_or_else(|_| clear_cookie())
}

/// `Set-Cookie` that deletes the session cookie.
pub fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static(
        "__Host-fi_session=; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Strict",
    )
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `POST /portal/v1/sessions {idToken}` + `X-Fi-Challenge` → 201 [`WebSignInResult`]; signed in:
/// the cookie is set, the token never appears in the body.
pub async fn sessions(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    ip: Option<Extension<ClientIp>>,
    request_headers: HeaderMap,
    key: IdempotencyKey,
    net: ClientNetwork,
    Body(req): Body<PortalSignIn>,
) -> Result<Response, ApiError> {
    turnstile::require(
        &core.turnstile,
        &state.pool,
        &request_headers,
        ip.map(|Extension(ClientIp(ip))| ip),
    )
    .await?;
    let (status, Json(result)) = identity::sessions(State(state), key, net, Body(req)).await?;
    let response = match result {
        PortalSignInResult::SignedIn {
            token,
            expires_at,
            viewer,
        } => {
            let cookie = session_cookie(&token, &expires_at);
            let mut r = (
                status,
                Json(WebSignInResult::SignedIn { expires_at, viewer }),
            )
                .into_response();
            r.headers_mut().insert(SET_COOKIE, cookie);
            r
        }
        PortalSignInResult::NeedsAccount {
            identity_ticket,
            identity_ticket_expires_at,
            email,
        } => (
            status,
            Json(WebSignInResult::NeedsAccount {
                identity_ticket,
                identity_ticket_expires_at,
                email,
            }),
        )
            .into_response(),
    };
    Ok(no_store(response))
}

/// `POST /portal/v1/accounts {identityTicket, locale?}` + `X-Fi-Challenge` → 201 [`WebSession`]
/// with the cookie set.
pub async fn accounts(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    ip: Option<Extension<ClientIp>>,
    request_headers: HeaderMap,
    key: IdempotencyKey,
    net: ClientNetwork,
    Body(req): Body<PortalAccountCreate>,
) -> Result<Response, ApiError> {
    fi_surfaces::signin::require_registration(&state, &mut *state.pool.acquire().await?).await?;
    turnstile::require(
        &core.turnstile,
        &state.pool,
        &request_headers,
        ip.map(|Extension(ClientIp(ip))| ip),
    )
    .await?;
    let (status, Json(issued)) = identity::accounts(State(state), key, net, Body(req)).await?;
    let cookie = session_cookie(&issued.token, &issued.expires_at);
    let mut r = (
        status,
        Json(WebSession {
            expires_at: issued.expires_at,
            viewer: issued.viewer,
        }),
    )
        .into_response();
    r.headers_mut().insert(SET_COOKIE, cookie);
    Ok(no_store(r))
}

/// `DELETE /portal/v1/sessions/current` → 204: revokes the session and clears the cookie (also
/// cleared when the session was already gone).
pub async fn sign_out(
    State(state): State<AppState>,
    auth: Result<PortalAuth, ApiError>,
) -> Response {
    let mut response = match auth {
        Ok(auth) => match identity::sign_out(State(state), auth).await {
            Ok(status) => status.into_response(),
            Err(e) => e.into_response(),
        },
        Err(e) => e.into_response(),
    };
    response.headers_mut().insert(SET_COOKIE, clear_cookie());
    no_store(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_has_every_attribute_and_a_bounded_life() {
        let in_a_day = fi_protocol::instant::format_unix_secs(
            fi_domain::UnixMs::now_system().millis() / 1_000 + 86_400,
        )
        .unwrap();
        let c = session_cookie("fi_ps_abc", &in_a_day);
        let text = c.to_str().unwrap();
        assert!(text.starts_with("__Host-fi_session=fi_ps_abc; Path=/; Max-Age="));
        for attr in ["Secure", "HttpOnly", "SameSite=Strict"] {
            assert!(text.contains(attr), "{attr}");
        }
        assert!(!text.contains("Domain"), "__Host- cookies have no Domain");
        let age: i64 = text
            .split("Max-Age=")
            .nth(1)
            .and_then(|r| r.split(';').next())
            .unwrap()
            .parse()
            .unwrap();
        assert!((86_000..=86_400).contains(&age), "{age}");
        assert!(clear_cookie().to_str().unwrap().contains("Max-Age=0"));
    }

    #[test]
    fn web_clients_name_a_version() {
        let mut h = HeaderMap::new();
        assert!(!web_client(&h));
        h.insert(headers::FI_CLIENT, HeaderValue::from_static("web/"));
        assert!(!web_client(&h));
        h.insert(
            headers::FI_CLIENT,
            HeaderValue::from_static("launcher/0.1.0"),
        );
        assert!(!web_client(&h));
        h.insert(headers::FI_CLIENT, HeaderValue::from_static("web/0.1.0"));
        assert!(web_client(&h));
    }
    #[tokio::test]
    async fn deferred_web_refuses_routes_and_cors_before_handlers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tower::ServiceExt;
        let core = Arc::new(Core::new(
            crate::config::CoreConfig::from_env(&[("FI_WEB_ENABLED", "false")]).unwrap(),
        ));
        let hits = Arc::new(AtomicUsize::new(0));
        let calls = hits.clone();
        let app = Router::new()
            .fallback(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::OK }
            })
            .layer(axum::middleware::from_fn_with_state(core, cors));
        for path in [
            "/portal/v1/sessions",
            "/portal/v1/accounts",
            "/portal/v1/me",
            "/core/v1/social/comments",
            "/core/v1/social/reports",
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .header(ORIGIN, "http://127.0.0.1:3120")
                        .body(axum::body::Body::from("invalid JSON; must never be read"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert!(!response.headers().contains_key(ACCESS_CONTROL_ALLOW_ORIGIN));
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/launcher/v1/info")
                    .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/launcher/v1/info")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn deferred_web_cookie_and_challenge_require_no_database_connection() {
        use axum::extract::FromRequestParts;
        let core = Arc::new(Core::new(
            crate::config::CoreConfig::from_env(&[("FI_WEB_ENABLED", "false")]).unwrap(),
        ));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://synthetic:synthetic@127.0.0.1:1/unreachable")
            .unwrap();
        let state = AppState {
            pool: pool.clone(),
            identity: None,
        };
        let (mut parts, _) = Request::builder()
            .header(
                axum::http::header::COOKIE,
                "__Host-fi_session=fi_ps_synthetic",
            )
            .body(())
            .unwrap()
            .into_parts();
        parts.extensions.insert(core.clone());
        assert!(
            matches!(crate::session::CoreSession::from_request_parts(&mut parts,&state).await,Err(error) if error.code==ErrorCode::PortalSessionRequired)
        );
        assert!(
            matches!(crate::turnstile::require(&core.turnstile,&pool,&HeaderMap::new(),None).await,Err(error) if error.code==ErrorCode::ServiceUnavailable)
        );
    }
}
