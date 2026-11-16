//! OD-79 short original-file startup check: thin authenticated HTTP, no inventory or world policy.
use crate::{
    errors::ApiError,
    extract::{IdempotencyKey, LauncherAuth},
    state::{AppState, Platform},
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Extension, Path, Query, Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
    routing::{get, post},
};
use fi_db::content;
use fi_protocol::{ErrorCode as C, content::*, routes::content as r};
use serde::Deserialize;
use uuid::Uuid;
/// Safe mapping without SQL, paths, payloads or account attribution in logs.
pub fn error(e: content::Error) -> ApiError {
    match e {
        content::Error::Invalid => ApiError::invalid("body", "invalid startup file observation"),
        content::Error::Conflict => ApiError::new(
            C::RequestIdempotencyKeyReused,
            "changed scan id, key or bytes",
        ),
        content::Error::NotFound => {
            ApiError::new(C::AccountsNotFound, "owned startup check not found")
        }
        content::Error::Unverified => {
            ApiError::new(C::AuthEmailUnverified, "verified identity required")
        }
        content::Error::Rate(n) => {
            ApiError::new(C::RequestRateLimited, "startup check quota exceeded").retry_after(n)
        }
        content::Error::Capacity | content::Error::Db(_) => {
            ApiError::new(C::ContentPolicyUnavailable, "startup check unavailable").retry_after(1)
        }
        content::Error::Denied(reason) => {
            let code = match reason {
                ContentReason::PolicyUnavailable => C::ContentPolicyUnavailable,
                ContentReason::PolicyChanged => C::ContentPolicyChanged,
                ContentReason::MissingScan | ContentReason::ScanExpired => {
                    C::ContentRecheckRequired
                }
                ContentReason::IncompatibleFiles => C::ContentIncompatible,
                _ => C::ContentInconclusive,
            };
            ApiError::new(code, "original-file startup check required")
                .with_details(serde_json::json!({"reason":reason}))
        }
    }
}
fn id(s: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(s)
        .ok()
        .filter(|v| !v.is_nil())
        .ok_or_else(|| ApiError::invalid("sessionId", "a UUID is required"))
}
fn headers(h: &HeaderMap, body: bool) -> Result<(), ApiError> {
    if h.get("X-Fi-Client")
        .and_then(|h| h.to_str().ok())
        .is_none_or(|c| !c.starts_with("infinity-launcher/") || c.len() > 96)
    {
        return Err(ApiError::invalid("X-Fi-Client", "launcher client required"));
    }
    if body
        && (h.contains_key("Content-Encoding")
            || h.get("Content-Type").and_then(|v| v.to_str().ok()) != Some("application/json"))
    {
        return Err(ApiError::invalid(
            "Content-Type",
            "uncompressed application/json required",
        ));
    }
    Ok(())
}
async fn private(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        "Cache-Control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}
#[derive(Deserialize)]
struct GameQuery {
    game: String,
}
async fn capabilities(
    State(s): State<AppState>,
    Extension(p): Extension<Platform>,
    auth: LauncherAuth,
    query: Result<Query<GameQuery>, axum::extract::rejection::QueryRejection>,
    h: HeaderMap,
) -> Result<Json<ContentCapabilities>, ApiError> {
    content::request_limit(&s.pool, auth.account_id, "read")
        .await
        .map_err(error)?;
    headers(&h, false)?;
    let game = query
        .map_err(|_| ApiError::invalid("game", "known game required"))?
        .0
        .game;
    let mut conn = s
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::unavailable())?;
    content::device_in(&mut conn, auth.account_id, auth.family_id)
        .await
        .map_err(error)?;
    Ok(Json(
        content::capabilities_in(&mut conn, &game, p.content_required)
            .await
            .map_err(error)?,
    ))
}
async fn permission(
    State(s): State<AppState>,
    auth: LauncherAuth,
    Path(session): Path<String>,
    h: HeaderMap,
) -> Result<Json<ContentPermission>, ApiError> {
    content::request_limit(&s.pool, auth.account_id, "permission")
        .await
        .map_err(error)?;
    headers(&h, false)?;
    let session = id(&session)?;
    let mut tx = s.pool.begin().await.map_err(|_| ApiError::unavailable())?;
    content::read_lock_in(&mut tx).await.map_err(error)?;
    let device = content::device_in(&mut tx, auth.account_id, auth.family_id)
        .await
        .map_err(error)?;
    let p = content::permission_in(&mut tx, auth.account_id, device, session)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(|_| ApiError::unavailable())?;
    Ok(Json(p))
}
async fn scan(
    State(s): State<AppState>,
    auth: LauncherAuth,
    key: Result<IdempotencyKey, ApiError>,
    req: Request,
) -> Result<Json<ContentScanAccepted>, ApiError> {
    content::request_limit(&s.pool, auth.account_id, "scan")
        .await
        .map_err(error)?;
    let key = key?;
    headers(req.headers(), true)?;
    let raw = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        to_bytes(req.into_body(), fi_content::SCAN_MAX),
    )
    .await
    .map_err(|_| ApiError::invalid("body", "body timeout"))?
    .map_err(|_| ApiError::new(C::RequestTooLarge, "startup check exceeds hard cap"))?;
    let body: ContentScan = serde_json::from_slice(&raw)
        .map_err(|_| ApiError::invalid("body", "invalid startup scan schema"))?;
    let mut tx = s.pool.begin().await.map_err(|_| ApiError::unavailable())?;
    let device = content::device_in(&mut tx, auth.account_id, auth.family_id)
        .await
        .map_err(error)?;
    let accepted = content::scan_in(&mut tx, auth.account_id, device, key.0, &body, &raw)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(|_| ApiError::unavailable())?;
    Ok(Json(accepted))
}
/// Only discovery, startup observation and own permission; all responses including errors no-store.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::CAPABILITIES, get(capabilities))
        .route(r::SCANS, post(scan))
        .route(r::PERMISSION, get(permission))
        .layer(axum::middleware::from_fn(private))
}
/// Ordinary session continuation. Login/leave/final launcher save handoff do not call this.
pub async fn session_guard_in(
    conn: &mut sqlx::PgConnection,
    p: &Platform,
    auth: &LauncherAuth,
    session: Uuid,
) -> Result<(), ApiError> {
    if !p.content_required {
        return Ok(());
    }
    // OD-113: only a Supporter + Hardcore session is bound to a content scan (CONTENT_API.md
    // §3); a dedicated host (OD-109), a modded session and every noob or free session are not.
    // An unknown or foreign session falls through to the ordinary checks (404).
    if content::session_bound_in(conn, auth.account_id, session)
        .await
        .map_err(error)?
        == Some(false)
    {
        return Ok(());
    }
    content::read_lock_in(conn).await.map_err(error)?;
    let device = content::device_in(conn, auth.account_id, auth.family_id)
        .await
        .map_err(error)?;
    content::require_in(conn, auth.account_id, device, session)
        .await
        .map_err(error)?;
    Ok(())
}
/// Startup admission, with original car database binding; hook package warnings stay separate.
/// OD-113, CR-S51: only a Hardcore profile admitted with a save line needs the scan and the
/// original car database; the caller skips this for a dedicated (OD-109) or modded admission.
pub async fn create_guard_in(
    conn: &mut sqlx::PgConnection,
    p: &Platform,
    auth: &LauncherAuth,
    req: &fi_protocol::saves::PlaySessionCreate,
) -> Result<(), ApiError> {
    // A game without the content scan has no manifest to scan (CR-S97: FM5).
    if !p.content_required || !fi_domain::GameRules::of(&req.game).content_scan {
        return Ok(());
    }
    if !content::create_bound_in(conn, auth.account_id, &req.game, &req.xuid)
        .await
        .map_err(error)?
    {
        return Ok(());
    }
    content::read_lock_in(conn).await.map_err(error)?;
    let device = content::device_in(conn, auth.account_id, auth.family_id)
        .await
        .map_err(error)?;
    content::require_create_in(conn, auth.account_id, device, req)
        .await
        .map_err(error)?;
    Ok(())
}
