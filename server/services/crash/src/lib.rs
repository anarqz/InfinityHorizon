//! OD-78: consented metadata-only diagnostics; no public moderation or attachment intake.
pub mod config;

use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use fi_db::crash;
use fi_protocol::ErrorCode as C;
use fi_protocol::crash::{CrashAccepted, CrashCapabilities, CrashCreate, CrashView};
use fi_protocol::routes::crash as r;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{IdempotencyKey, LauncherAuth};
use fi_surfaces::state::AppState;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

fn error(e: crash::Error) -> ApiError {
    let retry = match &e {
        crash::Error::Rate(v) => *v,
        _ => 1,
    };
    let (code, message) = match e {
        crash::Error::Conflict => (C::RequestIdempotencyKeyReused, "changed id, key or report"),
        crash::Error::Context => (C::CrashContextInvalid, "invalid incident or review context"),
        crash::Error::NotFound => (C::CrashNotFound, "report not found"),
        crash::Error::Unverified => (C::AuthEmailUnverified, "verified identity required"),
        crash::Error::Rate(_) => (C::RequestRateLimited, "crash request quota exceeded"),
        crash::Error::Db(_) | crash::Error::Capacity => {
            (C::ServiceUnavailable, "crash metadata unavailable")
        }
    };
    let e = ApiError::new(code, message);
    if matches!(code, C::RequestRateLimited | C::ServiceUnavailable) {
        e.retry_after(retry)
    } else {
        e
    }
}
fn disabled() -> ApiError {
    ApiError::new(
        C::CrashDumpsDisabled,
        "dump intake is disabled; choose metadata-only submission separately",
    )
}
fn id(s: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(s)
        .ok()
        .filter(|i| !i.is_nil())
        .ok_or_else(|| ApiError::invalid("id", "a UUID is required"))
}
fn media(h: &HeaderMap) -> Result<(), ApiError> {
    if h.contains_key("content-encoding")
        || h.get("content-type").and_then(|v| v.to_str().ok()) != Some("application/json")
    {
        return Err(ApiError::invalid(
            "Content-Type",
            "uncompressed application/json required",
        ));
    }
    Ok(())
}
fn client(headers: &HeaderMap) -> Result<(), ApiError> {
    let client = headers.get("X-Fi-Client").and_then(|v| v.to_str().ok());
    if client.is_none_or(|c| !c.starts_with("infinity-launcher/") || c.len() > 96) {
        return Err(ApiError::invalid("X-Fi-Client", "launcher client required"));
    }
    Ok(())
}
async fn private(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    response.headers_mut().insert(
        "Cache-Control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
async fn capabilities(
    State(s): State<AppState>,
    auth: LauncherAuth,
    headers: HeaderMap,
) -> Result<Json<CrashCapabilities>, ApiError> {
    crash::request_limit(&s.pool, auth.account_id, "read")
        .await
        .map_err(error)?;
    client(&headers)?;
    Ok(Json(CrashCapabilities {
        schema_version: 1,
        metadata_enabled: true,
        minidumps_enabled: false,
        max_metadata_bytes: fi_crash::METADATA_MAX as u32,
        max_observations: fi_crash::OBSERVATIONS_MAX as u32,
    }))
}
async fn create(
    State(s): State<AppState>,
    auth: LauncherAuth,
    key: Result<IdempotencyKey, ApiError>,
    req: Request,
) -> Result<(StatusCode, Json<CrashAccepted>), ApiError> {
    crash::request_limit(&s.pool, auth.account_id, "metadata")
        .await
        .map_err(error)?;
    let key = key?;
    client(req.headers())?;
    media(req.headers())?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(req.into_body(), fi_crash::METADATA_MAX),
    )
    .await
    .map_err(|_| ApiError::invalid("body", "body timeout"))?
    .map_err(|_| ApiError::new(C::RequestTooLarge, "metadata exceeds hard cap"))?;
    let body: CrashCreate = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::invalid("body", "invalid metadata schema"))?;
    let parsed = fi_crash::validate(&body).map_err(|field| {
        if field == "dump_disabled" {
            disabled()
        } else {
            ApiError::invalid(field, "invalid metadata field")
        }
    })?;
    let mut tx = s.pool.begin().await.map_err(|_| ApiError::unavailable())?;
    let accepted = crash::accept_in(&mut tx, auth.account_id, key.0, &parsed)
        .await
        .map_err(error)?;
    tx.commit().await.map_err(|_| ApiError::unavailable())?;
    Ok((StatusCode::ACCEPTED, Json(accepted)))
}
/// Compatibility refusal only. No Request/Body extractor: never poll/buffer/hash upload bytes.
async fn disabled_upload(
    State(s): State<AppState>,
    auth: LauncherAuth,
    key: Result<IdempotencyKey, ApiError>,
    Path(report): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    crash::request_limit(&s.pool, auth.account_id, "upload")
        .await
        .map_err(error)?;
    key?;
    id(&report)?;
    client(&headers)?;
    Err(disabled())
}
async fn view(
    State(s): State<AppState>,
    auth: LauncherAuth,
    Path(report): Path<String>,
    headers: HeaderMap,
) -> Result<Json<CrashView>, ApiError> {
    crash::request_limit(&s.pool, auth.account_id, "read")
        .await
        .map_err(error)?;
    client(&headers)?;
    let mut conn = s
        .pool
        .acquire()
        .await
        .map_err(|_| ApiError::unavailable())?;
    crash::view(&mut conn, auth.account_id, id(&report)?)
        .await
        .map(Json)
        .map_err(error)
}
/// Separate authenticated API, bounded buffering, standard dependency/schema health probes.
pub fn router(
    state: AppState,
    edge: Option<std::sync::Arc<fi_runtime::http::EdgeSecret>>,
) -> Router {
    let control_pool = state.pool.clone();
    let pool = state.pool.clone();
    let ready = move || {
        let pool = pool.clone();
        async move {
            fi_db::ping(&pool)
                .await
                .map_err(|_| "crash database unavailable".to_owned())?;
            fi_db::migrate::check_applied(&pool)
                .await
                .map(|_| ())
                .map_err(|_| "crash schema unavailable".to_owned())
        }
    };
    let routes = routes(state).merge(fi_runtime::http::health_router(ready));
    fi_surfaces::extract::origin_locked(
        fi_surfaces::online_control::protect(routes, control_pool),
        edge,
    )
}

fn routes(state: AppState) -> Router {
    Router::new()
        .route(r::CAPABILITIES, get(capabilities))
        .route(r::REPORTS, post(create))
        .route(r::REPORT, get(view))
        .route(r::MINIDUMP, put(disabled_upload))
        .layer(axum::middleware::from_fn(private))
        .layer(tower::limit::ConcurrencyLimitLayer::new(2))
        .with_state(state)
}

/// `/crash/v1` with its own limits, behind `online`, for a process that mounts several surfaces
/// (ADR 0023 §1, `fi-infinity`): no health, meta or origin lock.
pub fn surface(state: AppState, online: &fi_surfaces::online_control::OnlineGuard) -> Router {
    online.guard(routes(state))
}

/// Explicit optional transport; default fake, no payload or destination logging.
pub struct Discord {
    endpoint: Option<reqwest::Url>,
    client: reqwest::Client,
}
impl Discord {
    /// Real destination must be explicitly supplied; redirects/custom hosts/credentials refused.
    pub fn new(destination: Option<&str>) -> Result<Self, &'static str> {
        let endpoint = destination
            .map(|s| reqwest::Url::parse(s).map_err(|_| "invalid webhook destination"))
            .transpose()?;
        if let Some(u) = &endpoint
            && (u.scheme() != "https"
                || u.host_str() != Some("discord.com")
                || u.port().is_some()
                || !u.path().starts_with("/api/webhooks/")
                || u.username() != ""
                || u.password().is_some()
                || u.query().is_some()
                || u.fragment().is_some())
        {
            return Err("invalid webhook destination");
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| "webhook transport unavailable")?;
        Ok(Self { endpoint, client })
    }
    /// Repository supplies only allowlisted redacted diagnostics. Response bodies discarded.
    pub async fn send(&self, payload: &Value) -> bool {
        match &self.endpoint {
            None => true,
            Some(u) => self
                .client
                .post(u.clone())
                .json(payload)
                .send()
                .await
                .is_ok_and(|r| r.status().is_success()),
        }
    }
}
/// Bounded diagnostic maintenance tick. NEVER calls the human-moderation writer.
pub async fn work(pool: &fi_db::PgPool, discord: &Discord) -> Result<u64, crash::Error> {
    let mut tx = pool.begin().await?;
    crash::retain_in(&mut tx).await?;
    let count = crash::analyze_in(&mut tx).await?;
    tx.commit().await?;
    for _ in 0..100 {
        let Some((id, generation, payload)) = crash::notification(pool).await? else {
            break;
        };
        let ok = discord.send(&payload).await;
        crash::notification_done(pool, id, generation, ok).await?;
    }
    Ok(count)
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    #[test]
    fn errors_have_bounded_retry_and_disabled_upload_is_not_an_expired_capability() {
        for (e, status, retry) in [
            (crash::Error::Rate(60), 429, Some("60")),
            (crash::Error::Rate(86400), 429, Some("86400")),
            (crash::Error::Capacity, 503, Some("1")),
            (crash::Error::Context, 409, None),
        ] {
            let r = error(e).into_response();
            assert_eq!(r.status().as_u16(), status);
            assert_eq!(
                r.headers().get("Retry-After").and_then(|v| v.to_str().ok()),
                retry
            );
        }
        assert_eq!(disabled().code, C::CrashDumpsDisabled);
    }
    #[test]
    fn webhook_destination_is_explicit_and_pinned() {
        assert!(Discord::new(None).is_ok());
        assert!(Discord::new(Some("https://discord.com/api/webhooks/123/synthetic")).is_ok());
        for u in [
            "http://discord.com/api/webhooks/x",
            "https://evil.example/api/webhooks/x",
            "https://discord.com/api/webhooks/x?secret=x",
            "https://u@discord.com/api/webhooks/x",
        ] {
            assert!(Discord::new(Some(u)).is_err());
        }
    }
    #[tokio::test]
    async fn fake_transport_never_needs_a_destination() {
        assert!(
            Discord::new(None)
                .unwrap()
                .send(&serde_json::json!({"allowed_mentions":{"parse":[]}}))
                .await
        );
    }
}
