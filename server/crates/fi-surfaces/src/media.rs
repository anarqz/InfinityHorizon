//! `/media/v1` (ACCOUNTS.md §5a, register CR-S77, OD-137): public images on the core origin. The
//! one route is a Storefront livery file's preview PNG by its random token, for the launcher's
//! Discord presence (`ProfilePresence.liveryImage`). No credentials; one 404 for everything that
//! may not be served; never a token or an address in a log.

use crate::errors::ApiError;
use crate::state::AppState;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use fi_db::{limits, storefront};
use fi_protocol::ErrorCode;
use fi_protocol::routes::media as r;

/// The media routes.
pub fn router() -> Router<AppState> {
    Router::new().route(r::PREVIEW, get(preview))
}

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::PortalNotFound, "not found")
}

/// `GET /media/v1/previews/{token}.png`: 200 `image/png` (`Cache-Control: public, max-age=300`,
/// `ETag`; a matching `If-None-Match` is 304), else 404 `PORTAL.NOT_FOUND`; 300 reads an hour per
/// existing token (429 `REQUEST.RATE_LIMITED`).
async fn preview(
    State(state): State<AppState>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let token = file
        .strip_suffix(".png")
        .and_then(storefront::parse_token)
        .ok_or_else(not_found)?;
    let mut conn = state.pool.acquire().await?;
    let bytes = storefront::public_preview_in(&mut conn, &token)
        .await?
        .ok_or_else(not_found)?;
    if let limits::Hit::Limited { retry_after_secs } = limits::hit_in(
        &mut conn,
        &storefront::preview_key(&token),
        &storefront::PREVIEW_READS,
    )
    .await?
    {
        return Err(
            ApiError::new(ErrorCode::RequestRateLimited, "too many reads, retry later")
                .retry_after(retry_after_secs.max(1) as u64),
        );
    }
    drop(conn);
    let tag = storefront::preview_etag(&bytes);
    let tag_value = HeaderValue::from_str(&tag).map_err(|_| ApiError::unavailable())?;
    let cache = HeaderValue::from_static("public, max-age=300");
    let matched = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == tag || t.trim() == "*"));
    if matched {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(ETAG, tag_value), (CACHE_CONTROL, cache)],
        )
            .into_response());
    }
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, HeaderValue::from_static("image/png")),
            (ETAG, tag_value),
            (CACHE_CONTROL, cache),
        ],
        bytes,
    )
        .into_response())
}
