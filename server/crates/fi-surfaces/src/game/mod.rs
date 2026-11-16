//! The game plane's platform surface (`/game/v1`, docs/contracts/GAME_PLATFORM.md §2): session
//! check, progression facts, presence, each behind the session token (`X-Fi-Session`) and the
//! pool check (`GAME.WRONG_POOL`), every answer stamped `X-Fi-Pool` and `X-Fi-Instance`. Built by
//! workstream A of the local environment v0; the Forza RPC surface of SER-M3 mounts next to it.
//!
//! The order of the checks is the contract's: the token's signature (anything wrong: 401
//! `AUTH.SESSION_EXPIRED`), then its pool (403 `GAME.WRONG_POOL`, before any other check), then
//! its `exp`, then the session's liveness (state `active`, this pool, before `game_until`), read
//! from Postgres at most every `FI_GAME_LIVENESS_CACHE_MS` per session through the instance's
//! [`cache::LivenessCache`] (derived state, never shared; ADR 0008 §4 amendment 2026-10-18).
//! Refusals are codes, never 5xx.
//!
//! Progression, presence and the session view answer every admitted ticket, with or without a
//! save line (OD-239, CR-S111: results are the universe's, keyed by the profile).
//!
//! The game's own proxied requests (GAME_PLATFORM.md §3a, SER-M3): [`proxy`] (the catch-all a
//! deployment mounts as its fallback), [`request`] (one request after the checks) and
//! [`freeroam`] (the free-roam hosts, step 5).

pub mod activity;
pub mod cache;
pub mod freeroam;
pub mod proxy;
pub mod request;
pub mod ticket;

/// Whether a result of `live`'s session goes unrecorded (OD-112 follow-up, GATE_API.md §2
/// "Modded sessions"): a modded session's board and universe results are answered as successful
/// posts and never written. Logs the `modded_result_skipped` counter (by `kind`; ids only).
pub fn modded_result_skipped(live: &fi_db::game::Liveness, kind: &'static str) -> bool {
    if live.modded {
        tracing::info!(event = "modded_result_skipped", kind, session_id = %live.session_id,
            "a modded session's result is not recorded");
    }
    live.modded
}

use crate::errors::ApiError;
use crate::extract::Body;
use crate::state::{AppState, Platform, session_keys};
use crate::views;
use axum::Router;
use axum::extract::{Extension, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fi_db::game::{self, Fact, Liveness};
use fi_protocol::game::{
    FI_INSTANCE, FI_POOL, GameSession, PROGRESSION_KINDS, ProgressionFact, ProgressionRecorded,
};
use fi_protocol::headers;
use fi_protocol::routes::game as g;
use uuid::Uuid;

/// The game surface of one pool (`FI_POOL`).
pub fn router() -> Router<AppState> {
    Router::new()
        .route(g::SESSION, get(session_view))
        .route(g::PROGRESSION, post(progression))
        .route(g::PRESENCE, post(presence))
        .layer(middleware::from_fn(stamp))
}

/// Stamps `X-Fi-Pool` and `X-Fi-Instance` on every answer of the surface, refusals included.
async fn stamp(request: Request, next: Next) -> Response {
    let platform = request.extensions().get::<Platform>().cloned();
    let mut response = next.run(request).await;
    if let Some(p) = platform {
        let h = response.headers_mut();
        if let Some(Ok(v)) = p.game_pool.as_deref().map(HeaderValue::from_str) {
            h.insert(FI_POOL, v);
        }
        if let Ok(v) = HeaderValue::from_str(&p.instance_id) {
            h.insert(FI_INSTANCE, v);
        }
    }
    response
}

/// A request with a valid session token of this pool for a session that is live now.
///
/// On a platform v1 deployment that installs [`ticket::Tickets`] (services/saves: verify-only
/// keys, `pool = vip`, `game ∈ FI_GAMES`) the check is [`ticket::Ticket`]'s; otherwise (the local
/// environment v0's `game-<pool>` deployments) the token is verified with the deployment's ring
/// against `FI_POOL`, in the same order.
pub struct GameAuth {
    /// The verified claims.
    pub claims: ticket::TicketClaims,
    /// The session as Postgres sees it.
    pub live: Liveness,
}

impl FromRequestParts<AppState> for GameAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if parts.extensions.get::<ticket::Tickets>().is_some() {
            let t = ticket::Ticket::from_request_parts(parts, state).await?;
            return Ok(GameAuth {
                claims: t.claims,
                live: t.live,
            });
        }
        let platform = parts
            .extensions
            .get::<Platform>()
            .cloned()
            .ok_or_else(ApiError::unavailable)?;
        let pool = platform
            .game_pool
            .clone()
            .ok_or_else(ApiError::unavailable)?;
        let verified = match ticket::header_token(parts) {
            Some(token) => ticket::verify_ring(session_keys(&platform)?, token),
            None => Err(fi_auth::session::SessionError::Invalid),
        };
        let t = ticket::admit_with_policy(
            &state.pool,
            verified,
            std::slice::from_ref(&pool),
            None,
            &platform.liveness,
            None,
            ticket::OnlinePolicy {
                content_required: platform.content_required,
                min_launcher: &platform.min_launcher,
            },
        )
        .await?;
        Ok(GameAuth {
            claims: t.claims,
            live: t.live,
        })
    }
}

/// `GET /game/v1/session` → 200: the session as the token and Postgres see it.
async fn session_view(
    Extension(platform): Extension<Platform>,
    auth: GameAuth,
) -> Result<axum::Json<GameSession>, ApiError> {
    Ok(axum::Json(GameSession {
        session_id: auth.live.session_id.to_string(),
        account_id: auth.live.account_id.to_string(),
        profile_id: auth.live.profile_id.to_string(),
        xuid: auth.claims.xuid,
        pool: auth.live.pool,
        tier: auth.claims.tier,
        mode: auth.claims.mode,
        game_until: auth.live.game_until.map(views::at).unwrap_or_default(),
        instance: platform.instance_id.clone(),
    }))
}

/// The id of a fact: `UUIDv5(profile id, the 16 bytes of X-Fh2-Request-Id)`, so a retried
/// request is the same fact on every instance.
pub fn event_id(profile: Uuid, request_id: Uuid) -> Uuid {
    Uuid::new_v5(&profile, request_id.as_bytes())
}

/// `POST /game/v1/progression` → 201 recorded, 200 a retry (GAME_PLATFORM.md §2.2).
async fn progression(
    State(state): State<AppState>,
    auth: GameAuth,
    headers: HeaderMap,
    Body(fact): Body<ProgressionFact>,
) -> Result<Response, ApiError> {
    let request_id = headers
        .get(headers::FH2_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v.trim()).ok())
        .filter(|u| !u.is_nil())
        .ok_or_else(|| ApiError::invalid("X-Fh2-Request-Id", "a UUID request id is required"))?;
    if !PROGRESSION_KINDS.contains(&fact.kind.as_str()) {
        return Err(ApiError::invalid("kind", "unknown progression kind"));
    }
    // OD-112 follow-up: a modded session's race results and scores answer as recorded and are
    // never written (`profile_progress` counts them otherwise).
    if matches!(fact.kind.as_str(), "race_result" | "score")
        && modded_result_skipped(&auth.live, "progression")
    {
        return Ok((
            StatusCode::CREATED,
            axum::Json(ProgressionRecorded {
                event_id: event_id(auth.live.profile_id, request_id).to_string(),
                recorded: true,
                recorded_at: views::at(fi_domain::UnixMs::now_system()),
            }),
        )
            .into_response());
    }
    let mut tx = state.pool.begin().await?;
    let recorded = game::record_progression_in(
        &mut tx,
        &Fact {
            event_id: event_id(auth.live.profile_id, request_id),
            account_id: auth.live.account_id,
            profile_id: auth.live.profile_id,
            session_id: Some(auth.live.session_id),
            kind: fact.kind,
            data: fact.data,
        },
    )
    .await?;
    tx.commit().await?;
    let status = if recorded.recorded {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        axum::Json(ProgressionRecorded {
            event_id: recorded.event_id.to_string(),
            recorded: recorded.recorded,
            recorded_at: views::at(recorded.recorded_at),
        }),
    )
        .into_response())
}

/// `POST /game/v1/presence` → 204: this pool's instance served the session.
async fn presence(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: GameAuth,
) -> Result<StatusCode, ApiError> {
    let mut conn = state.pool.acquire().await?;
    game::presence_in(
        &mut conn,
        auth.live.session_id,
        auth.live.profile_id,
        &auth.live.pool,
        &platform.instance_id,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retried_request_is_the_same_fact_and_profiles_never_share_one() {
        let (p1, p2, r) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        assert_eq!(event_id(p1, r), event_id(p1, r));
        assert_ne!(event_id(p1, r), event_id(p2, r));
        assert_eq!(event_id(p1, r).get_version_num(), 5);
    }
}
