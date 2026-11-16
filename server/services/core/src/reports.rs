//! Reports from the game (CORE_API.md §4.3; OD-63, register CR-L25): the overlay reports a
//! player of the world's roster, and the game's own player menus any player card (OD-193,
//! CR-S90), through the launcher, `POST /launcher/v1/reports` with the launcher's access token and
//! an `Idempotency-Key`. Neither can solve Turnstile: the launcher session stands in for it, and a
//! world named must be one the two shared in the last 30 minutes
//! ([`fi_db::moderation::game_report_in`]). Any account may report (no Supporter gate).
//!
//! The report lands in the social reports' store (`source = game`) and counts toward the
//! three-report rule unchanged (ADR 0204). One report per reporter account and target profile a
//! day: a repeat answers the first `id`. Limits: the per-account write bucket, then `report.game`
//! (10 an hour per account, Postgres), counted before anything is looked up (a replay of the same
//! key is answered first and spends nothing). A request holds one pooled connection: the replay
//! check and the limit run on it, then its transaction. Logs carry no XUID.

use crate::Core;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use fi_db::limits::{self, Hit, Recorded};
use fi_db::moderation::{self, GAME_REPORTS, GameReported};
use fi_protocol::ErrorCode;
use fi_protocol::core::{GameReport, ReportAccepted};
use fi_protocol::routes::launcher as lr;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, IdempotencyKey, LauncherAuth};
use fi_surfaces::state::AppState;
use serde_json::{Value, json};
use sqlx::Connection;
use std::sync::Arc;
use uuid::Uuid;

/// The idempotency route name of a report from the game.
const ROUTE: &str = "POST /launcher/v1/reports";

/// The launcher's report route.
pub fn router() -> Router<AppState> {
    Router::new().route(lr::REPORTS, post(report))
}

fn not_found() -> ApiError {
    ApiError::new(ErrorCode::PortalNotFound, "not found")
}

/// The answer of an `Idempotency-Key` already used: the first 202 for the same request,
/// `REQUEST.IDEMPOTENCY_KEY_REUSED` for another; `None` for a fresh key.
fn replayed(r: Recorded) -> Result<Option<(StatusCode, Json<Value>)>, ApiError> {
    match r {
        Recorded::Same(r) => Ok(Some((
            StatusCode::ACCEPTED,
            Json(r.response.unwrap_or_default()),
        ))),
        Recorded::Different => Err(ApiError::new(
            ErrorCode::RequestIdempotencyKeyReused,
            "this Idempotency-Key was used for another request",
        )),
        Recorded::None => Ok(None),
    }
}

/// `POST /launcher/v1/reports {game, xuid, reason, worldId?}` → 202 [`ReportAccepted`].
pub async fn report(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Body(req): Body<GameReport>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !crate::valid_game(&req.game) {
        return Err(ApiError::invalid("game", "a game id such as fh2"));
    }
    let xuid = req
        .xuid
        .parse::<u64>()
        .ok()
        .filter(|x| fi_domain::Xuid::new(*x).is_ok())
        .ok_or_else(|| ApiError::invalid("xuid", "a decimal XUID"))?;
    if !matches!(req.reason.as_str(), "cheating" | "abuse" | "other") {
        return Err(ApiError::invalid("reason", "cheating, abuse or other"));
    }
    let world_id = req
        .world_id
        .as_deref()
        .map(|w| Uuid::parse_str(w).map_err(|_| ApiError::invalid("worldId", "a world id")))
        .transpose()?;
    core.account_write(auth.account_id)?;
    let subject = format!("account:{}", auth.account_id);
    let idem = key.of(
        ROUTE,
        &json!({"game": req.game, "xuid": xuid.to_string(), "reason": req.reason,
                "worldId": world_id.map(|w| w.to_string())}),
    );
    let mut conn = state.pool.acquire().await?;
    // A replay answers the first outcome and spends nothing.
    if let Some(replay) = replayed(limits::recorded_in(&mut conn, &subject, &idem).await?)? {
        return Ok(replay);
    }
    // Counted in its own statement before any lookup: unknown, ineligible and repeated reports
    // count too, so the rule cannot be probed for who shared a world or who exists.
    let rate_key = state
        .id()
        .pepper()?
        .rate_limit_key(GAME_REPORTS.name, &subject);
    if let Hit::Limited { retry_after_secs } =
        limits::hit_in(&mut conn, &rate_key, &GAME_REPORTS).await?
    {
        return Err(ApiError::new(
            ErrorCode::RequestRateLimited,
            "too many reports, retry later",
        )
        .retry_after(retry_after_secs.max(1) as u64));
    }
    let mut tx = conn.begin().await?;
    // Again inside the transaction: a duplicate that committed meanwhile is replayed.
    if let Some(replay) = replayed(limits::recorded_in(&mut tx, &subject, &idem).await?)? {
        return Ok(replay);
    }
    let filed = moderation::game_report_in(
        &mut tx,
        &moderation::GameReport {
            reporter: auth.account_id,
            game: &req.game,
            xuid,
            reason: &req.reason,
            world_id,
        },
    )
    .await?;
    let (id, hidden) = match filed {
        GameReported::Unknown | GameReported::NotShared => return Err(not_found()),
        GameReported::Own => {
            return Err(ApiError::invalid("xuid", "a profile of your own account"));
        }
        GameReported::Repeat(id) => (id, false),
        GameReported::Filed(r) => (r.id, r.hidden),
    };
    let body = serde_json::to_value(ReportAccepted { id: id.to_string() })
        .map_err(|_| ApiError::unavailable())?;
    limits::record_with_response_in(&mut tx, &subject, &idem, 202, Some(id), Some(&body)).await?;
    tx.commit().await?;
    if hidden {
        tracing::info!(
            kind = "player",
            source = "game",
            "a target reached the report threshold and is hidden pending review"
        );
    }
    Ok((StatusCode::ACCEPTED, Json(body)))
}
