//! Voice mutes from the game (CORE_API.md §4.4; register CR-S68, OD-154, OD-163): the overlay's
//! mute or unmute of a player's voice through the launcher, `POST /launcher/v1/voice/mutes` with
//! the launcher's access token. The relay enforces each player's own mutes; this route feeds the
//! community mute and the moderators' history ([`fi_db::voice::mute_in`]).
//!
//! Idempotent by state (no `Idempotency-Key`): a repeat of the caller's current state toward the
//! target's account answers 202 and writes nothing. Limits: the per-account write bucket, then
//! `voice.mute` (60 an hour per account, Postgres), counted before anything is looked up, so the
//! route cannot be probed for who shared a world. One pooled connection: the limit runs on it,
//! then its transaction. Logs carry no XUID.

use crate::Core;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use fi_db::limits::{self, Hit};
use fi_db::voice::{self, Muted, VOICE_MUTES};
use fi_protocol::ErrorCode;
use fi_protocol::core::{VOICE_MUTE_ACTIONS, VoiceMute, VoiceMuteAccepted};
use fi_protocol::routes::launcher as lr;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, LauncherAuth};
use fi_surfaces::state::AppState;
use sqlx::Connection;
use std::sync::Arc;
use uuid::Uuid;

/// The launcher's voice mute route.
pub fn router() -> Router<AppState> {
    Router::new().route(lr::VOICE_MUTES, post(mute))
}

/// `POST /launcher/v1/voice/mutes {xuid, action, worldId}` → 202 [`VoiceMuteAccepted`].
pub async fn mute(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
    auth: LauncherAuth,
    Body(req): Body<VoiceMute>,
) -> Result<(StatusCode, Json<VoiceMuteAccepted>), ApiError> {
    let xuid = req
        .xuid
        .parse::<u64>()
        .ok()
        .filter(|x| fi_domain::Xuid::new(*x).is_ok() && x.to_string() == req.xuid)
        .ok_or_else(|| ApiError::invalid("xuid", "a decimal XUID"))?;
    if !VOICE_MUTE_ACTIONS.contains(&req.action.as_str()) {
        return Err(ApiError::invalid("action", "muted or unmuted"));
    }
    let world_id =
        Uuid::parse_str(&req.world_id).map_err(|_| ApiError::invalid("worldId", "a world id"))?;
    core.account_write(auth.account_id)?;
    let subject = format!("account:{}", auth.account_id);
    let mut conn = state.pool.acquire().await?;
    // Counted in its own statement before any lookup (unknown, ineligible and repeated mutes
    // count too).
    let rate_key = state
        .id()
        .pepper()?
        .rate_limit_key(VOICE_MUTES.name, &subject);
    if let Hit::Limited { retry_after_secs } =
        limits::hit_in(&mut conn, &rate_key, &VOICE_MUTES).await?
    {
        return Err(ApiError::new(
            ErrorCode::VoiceRateLimited,
            "too many voice mutes, retry later",
        )
        .retry_after(retry_after_secs.max(1) as u64));
    }
    let mut tx = conn.begin().await?;
    let done = voice::mute_in(
        &mut tx,
        &voice::Mute {
            muter: auth.account_id,
            xuid,
            action: &req.action,
            world_id,
        },
    )
    .await?;
    match done {
        Muted::NotShared => {
            return Err(ApiError::new(
                ErrorCode::VoiceNotShared,
                "no world shared with that player in the last 30 minutes",
            ));
        }
        Muted::Own => return Err(ApiError::invalid("xuid", "a profile of your own account")),
        Muted::Unchanged => {}
        Muted::Changed { community } => {
            if community.is_some() {
                tracing::info!(kind = "voice", "a community voice mute started");
            }
        }
    }
    tx.commit().await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(VoiceMuteAccepted {
            xuid: xuid.to_string(),
            action: req.action,
        }),
    ))
}
