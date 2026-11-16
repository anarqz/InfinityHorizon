//! `/gate/v1` (GATE_API.md §2-§7): admission, poll, heartbeat, ticket, leave, the public queue
//! summary and the internal pool list. Thin: every decision is one transaction of
//! [`fi_db::gate`]; tickets are minted after commit with the `kid` and `exp` the transaction
//! recorded; each request holds one pooled connection at a time (services/AGENTS.md).
//!
//! Limits ([`rate_limit`], ADR 0903 §3, GATE_API.md §8): admission and ticket refresh count per
//! account in Postgres (`fi_db::gate::ADMIT_LIMIT`, `TICKET_LIMIT`: 6/min, exact across
//! instances, since each admission takes its pool's advisory lock), the other session routes in a
//! per-instance bucket per account (60/min), and the public queue summary per client network
//! (60/min). 429 `REQUEST.RATE_LIMITED` with `Retry-After`.

use crate::config::InternalAccess;
use axum::Router;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fi_db::PgSaveStore;
use fi_db::admission::{self, Admitted, Outcome, SaveLinePolicy};
use fi_db::gate::{self, AdmitRequest};
use fi_domain::PlaySessionId;
use fi_protocol::ErrorCode;
use fi_protocol::gate::{
    GateSession, Heartbeat, HeartbeatResponse, Leave, LeaveReason, OnlineQueue, PoolQueue,
    PoolStatus, Pools, QueuePlace, QueueSummary, SessionAdmission, Ticket, TicketResponse,
};
use fi_protocol::routes::gate as r;
use fi_protocol::saves::Pool;
use fi_runtime::ratelimit::{Decision, Rate, TokenBuckets};
use fi_saves::SaveStore;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{Body, IdempotencyKey, LauncherAuth};
use fi_surfaces::extract::{ClientNetwork, bearer};
use fi_surfaces::launcher::saves::{head_body, session_profile_of};
use fi_surfaces::launcher::sessions::{
    self as alias, check_create, fence, finish_refused, game_plane, gate_error, heartbeat_every_ms,
    launch_ticket, lease_body, mint_ticket, pool_body, record_ticket_in,
};
use fi_surfaces::state::{AppState, Platform, now_secs};
use fi_surfaces::views;
use serde::Deserialize;
use sqlx::PgConnection;
use std::sync::Arc;

/// The gate's per-instance token buckets (derived state: losing them costs a fresh bucket).
#[derive(Debug, Default)]
pub struct Limits {
    buckets: TokenBuckets,
}

/// Session reads, heartbeats and leaves per account (a queued launcher polls every 10 s, a
/// playing one heartbeats every 60 s).
pub const SESSION_RATE: Rate = Rate::per_minute(60);
/// `GET /gate/v1/queue` per client network (the public read rate of ADR 0903 §3).
pub const QUEUE_RATE: Rate = Rate::per_minute(60);

/// Which limit a matched route takes.
enum Class {
    /// Postgres, per account.
    Exact(&'static fi_db::limits::Limit),
    /// In-app bucket, per account.
    Session,
    /// In-app bucket, per client network.
    Public,
}

fn class(method: &axum::http::Method, route: &str) -> Option<Class> {
    use axum::http::Method;
    use fi_protocol::routes::launcher as l;
    match (method, route) {
        (&Method::POST, r::SESSIONS | l::SESSIONS) => Some(Class::Exact(&gate::ADMIT_LIMIT)),
        (&Method::POST, r::SESSION_TICKET | l::SESSION_TOKEN) => {
            Some(Class::Exact(&gate::TICKET_LIMIT))
        }
        (&Method::GET, r::QUEUE) => Some(Class::Public),
        (_, r::INTERNAL_POOLS) => None,
        (_, route) if route.starts_with(r::SESSIONS) || route.starts_with(l::SESSIONS) => {
            Some(Class::Session)
        }
        _ => None,
    }
}

fn limited(retry_after_secs: u64) -> Response {
    ApiError::new(
        ErrorCode::RequestRateLimited,
        "too many requests, retry later",
    )
    .retry_after(retry_after_secs.max(1))
    .into_response()
}

/// The account of a valid launcher access token (signature and expiry only).
fn account_of(state: &AppState, headers: &axum::http::HeaderMap) -> Option<uuid::Uuid> {
    let token = bearer(headers)?;
    let verified = state
        .identity
        .as_deref()?
        .access_keys
        .verify(token, fi_auth::access::Audience::Launcher, now_secs())
        .ok()?;
    uuid::Uuid::parse_str(&verified.account_id).ok()
}

/// Middleware on the gate's routes (after routing, so the matched route is known): the limits of
/// the module documentation. The account comes from the access token's signature alone (no
/// database read); a request without a valid token is not counted here and gets its 401 from the
/// route.
pub async fn rate_limit(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_owned());
    let (Some(route), Some(limits)) = (route, request.extensions().get::<Arc<Limits>>().cloned())
    else {
        return next.run(request).await;
    };
    let Some(class) = class(request.method(), &route) else {
        return next.run(request).await;
    };
    // Read before any await: the request body is not `Sync`, so no borrow of it may cross one.
    let account = account_of(&state, request.headers());
    let network = request
        .extensions()
        .get::<ClientNetwork>()
        .map(|n| n.0.clone())
        .unwrap_or_else(|| "unknown".into());
    match class {
        Class::Exact(limit) => {
            if let Some(account) = account {
                match gate::rate_limit(&state.pool, limit, account).await {
                    Ok(fi_db::limits::Hit::Allowed) => {}
                    Ok(fi_db::limits::Hit::Limited { retry_after_secs }) => {
                        tracing::info!(limit = limit.name, "rate limited");
                        return limited(u64::try_from(retry_after_secs).unwrap_or(1));
                    }
                    Err(e) => return ApiError::from(e).into_response(),
                }
            }
        }
        Class::Session => {
            if let Some(account) = account
                && let Decision::Limited { retry_after_secs } = limits
                    .buckets
                    .take(&format!("session:{account}"), SESSION_RATE)
            {
                tracing::info!(limit = "gate.session", "rate limited");
                return limited(retry_after_secs);
            }
        }
        Class::Public => {
            if let Decision::Limited { retry_after_secs } =
                limits.buckets.take(&format!("queue:{network}"), QUEUE_RATE)
            {
                tracing::info!(limit = "gate.queue", "rate limited");
                return limited(retry_after_secs);
            }
        }
    }
    next.run(request).await
}

/// The `/gate/v1` routes (the internal list only with `internal = Open`).
pub fn router(internal: InternalAccess) -> Router<AppState> {
    Router::new()
        .route(r::SESSIONS, post(create))
        .route(r::SESSION, get(poll))
        .route(r::SESSION_HEARTBEAT, post(heartbeat))
        .route(r::SESSION_TICKET, post(ticket))
        .route(r::SESSION_LEAVE, post(leave))
        .route(r::QUEUE, get(queue))
        .route(
            r::INTERNAL_POOLS,
            get(move |state, platform| internal_pools(internal, state, platform)),
        )
}

fn ticket_body(p: &Platform, a: &Admitted) -> Result<Ticket, ApiError> {
    let t = mint_ticket(p, a)?;
    Ok(Ticket {
        token: t.token,
        expires_at: views::at_secs(t.expires_at),
        heartbeat_every_ms: heartbeat_every_ms(a),
    })
}

/// The active body: the save head is read after commit, on the request's only connection.
async fn active_body(
    state: &AppState,
    p: &Platform,
    a: &Admitted,
) -> Result<GateSession, ApiError> {
    let save = match a.lease {
        Some(_) => {
            let head = PgSaveStore::new(state.pool.clone())
                .head(a.profile_id)
                .await?;
            Some(head_body(&state.pool, p, &a.game, a.account_id, &head).await?)
        }
        None => None,
    };
    Ok(GateSession::Active {
        session_id: a.session_id.to_string(),
        game: a.game.clone(),
        pool: pool_body(a),
        ticket: ticket_body(p, a)?,
        launch_ticket: launch_ticket(state, a)?,
        lease: lease_body(a),
        save,
        game_until: views::at(a.game_until),
        max_until: views::at(a.max_until),
    })
}

/// What a queued answer needs from the transaction (read before it commits).
struct QueuedView {
    session: PlaySessionId,
    game: String,
    pool: Pool,
    queue: QueuePlace,
}

async fn queued_view_in(
    conn: &mut PgConnection,
    session: PlaySessionId,
    position: u32,
) -> Result<QueuedView, ApiError> {
    let place = gate::queue_place_in(conn, session, position).await?;
    Ok(QueuedView {
        session,
        game: place.game,
        pool: Pool {
            name: place.pool,
            tier: place.tier,
            server: place.server,
        },
        queue: QueuePlace {
            position,
            ahead: position.saturating_sub(1),
            eta_seconds: place.eta_seconds,
            retry_after_seconds: gate::queue_retry_after_secs(position, place.eta_seconds),
        },
    })
}

fn queued_body(v: QueuedView) -> GateSession {
    GateSession::Queued {
        session_id: v.session.to_string(),
        game: v.game,
        pool: v.pool,
        queue: v.queue,
    }
}

/// Records the ticket of an admission, or reads the queue view, before commit.
enum Decided {
    Active(Box<Admitted>),
    Queued(QueuedView),
}

async fn decided_in(
    conn: &mut PgConnection,
    p: &Platform,
    outcome: Outcome,
) -> Result<Decided, ApiError> {
    Ok(match outcome {
        Outcome::Active(a) => {
            record_ticket_in(conn, p, &a).await?;
            Decided::Active(a)
        }
        Outcome::Queued {
            session_id,
            position,
        } => Decided::Queued(queued_view_in(conn, session_id, position).await?),
    })
}

async fn respond(
    state: &AppState,
    p: &Platform,
    decided: Decided,
    active: StatusCode,
    queued: StatusCode,
) -> Result<Response, ApiError> {
    Ok(match decided {
        Decided::Active(a) => {
            (active, axum::Json(active_body(state, p, &a).await?)).into_response()
        }
        Decided::Queued(v) => (queued, axum::Json(queued_body(v))).into_response(),
    })
}

/// `POST /gate/v1/sessions` → 201 active, 202 queued (GATE_API.md §2).
async fn create(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Body(body): Body<SessionAdmission>,
) -> Result<Response, ApiError> {
    // The checks shared with the /launcher/v1 alias read the body without OD-109's `dedicated`.
    let dedicated = body.dedicated;
    // Absent `carDbSha256` is a dedicated session's alone (OD-109); an empty value keeps its
    // meaning (no car database declared) for every session.
    if !dedicated && body.car_db_sha256.is_none() {
        return Err(ApiError::invalid(
            "carDbSha256",
            "required unless the session is dedicated",
        ));
    }
    let req = body.play();
    let checked = check_create(&state, &auth, &key, &req, dedicated)?;
    let profile = session_profile_of(&state.pool, auth.account_id, &req.xuid, &req.game).await?;
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    fi_db::launcher_policy::require_create_in(
        &mut tx,
        auth.account_id,
        profile.profile_id.as_uuid(),
        checked.session.as_uuid(),
        &req.game,
        &req.launcher_version,
        &state.id().info.min_launcher,
    )
    .await
    .map_err(fi_surfaces::launcher_policy::error)?;
    // OD-109: a dedicated session runs no game, so no content scan binds it; OD-113: neither
    // does a modded one (never Hardcore; its admission refuses a Hardcore profile).
    if !dedicated && !body.modded {
        fi_surfaces::content::create_guard_in(&mut tx, &platform, &auth, &req).await?;
    }
    let device = admission::device_of_family_in(&mut tx, auth.account_id, auth.family_id)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::AuthTokenInvalid, "invalid access token"))?;
    let outcome = gate::admit_in(
        &mut tx,
        &AdmitRequest {
            session_id: checked.session,
            account_id: auth.account_id,
            game: &req.game,
            profile: &profile,
            device: &device,
            routing: &platform.routing,
            launcher_version: &req.launcher_version,
            nonce: &checked.nonce,
            car_db: checked.car_db,
            take_over: req.take_over,
            ttl: platform.slot_ttl(),
            save_line: SaveLinePolicy::VipPools,
            dedicated,
            modded: body.modded,
        },
    )
    .await;
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return Err(finish_refused(tx, e).await?),
    };
    if !body.features.is_empty() {
        gate::record_features_in(&mut tx, checked.session, &body.features).await?;
    }
    let decided = decided_in(&mut tx, &platform, outcome).await?;
    tx.commit().await?;
    if let Decided::Active(a) = &decided {
        tracing::info!(session = %a.session_id, game = %a.game, pool = %a.pool, save_line = a.lease.is_some(), dedicated = a.dedicated, "admitted");
    }
    respond(
        &state,
        &platform,
        decided,
        StatusCode::CREATED,
        StatusCode::ACCEPTED,
    )
    .await
}

/// `GET /gate/v1/sessions/{id}` → 200 with either body; 404 for another account's session.
async fn poll(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let session = alias::session_id(&id)?;
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    // The floor applies at creation and to a session still waiting to be admitted, never to one
    // already playing (OD-121 follow-up "Admission only").
    fi_db::launcher_policy::require_queued_in(
        &mut tx,
        auth.account_id,
        session.as_uuid(),
        &state.id().info.min_launcher,
    )
    .await
    .map_err(fi_surfaces::launcher_policy::error)?;
    fi_surfaces::content::session_guard_in(&mut tx, &platform, &auth, session.as_uuid()).await?;
    let outcome = match gate::poll_in(&mut tx, session, auth.account_id, platform.slot_ttl()).await
    {
        Ok(o) => o,
        Err(e) => return Err(finish_refused(tx, e).await?),
    };
    let decided = decided_in(&mut tx, &platform, outcome).await?;
    tx.commit().await?;
    respond(&state, &platform, decided, StatusCode::OK, StatusCode::OK).await
}

/// `POST /gate/v1/sessions/{id}/heartbeat` → 200 (GATE_API.md §4).
async fn heartbeat(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    Path(id): Path<String>,
    Body(req): Body<Heartbeat>,
) -> Result<axum::Json<HeartbeatResponse>, ApiError> {
    let session = alias::session_id(&id)?;
    let token = req.fence_token.map(fence).transpose()?;
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    fi_surfaces::content::session_guard_in(&mut tx, &platform, &auth, session.as_uuid()).await?;
    let a = match gate::heartbeat_in(
        &mut tx,
        session,
        auth.account_id,
        token,
        platform.slot_ttl(),
    )
    .await
    {
        Ok(a) => a,
        Err(e) => return Err(finish_refused(tx, e).await?),
    };
    record_ticket_in(&mut tx, &platform, &a).await?;
    tx.commit().await?;
    // OD-121: a session that reported activity before is judged again at each heartbeat, so a
    // hook that stops reporting turns idle by time (observation only; bounded, never fails this).
    fi_surfaces::game::activity::evaluate(&state.pool, session.as_uuid()).await;
    Ok(axum::Json(HeartbeatResponse {
        lease: lease_body(&a),
        ticket: ticket_body(&platform, &a)?,
        game_plane: game_plane(&a),
    }))
}

/// `POST /gate/v1/sessions/{id}/ticket` → 200 `{ticket}`: no lease renewal.
async fn ticket(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    Path(id): Path<String>,
) -> Result<axum::Json<TicketResponse>, ApiError> {
    let session = alias::session_id(&id)?;
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    fi_surfaces::content::session_guard_in(&mut tx, &platform, &auth, session.as_uuid()).await?;
    let a = gate::ticket_in(&mut tx, session, auth.account_id)
        .await
        .map_err(gate_error)?;
    record_ticket_in(&mut tx, &platform, &a).await?;
    tx.commit().await?;
    Ok(axum::Json(TicketResponse {
        ticket: ticket_body(&platform, &a)?,
    }))
}

/// `POST /gate/v1/sessions/{id}/leave` → 204, idempotent (GATE_API.md §5).
async fn leave(
    State(state): State<AppState>,
    auth: LauncherAuth,
    Path(id): Path<String>,
    Body(req): Body<Leave>,
) -> Result<StatusCode, ApiError> {
    let session = alias::session_id(&id)?;
    let token = req.fence_token.map(fence).transpose()?;
    let reason = match req.reason {
        LeaveReason::GameExit => "game_exit",
        LeaveReason::Player => "player",
    };
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    if let Err(e) = gate::leave_in(
        &mut tx,
        session,
        auth.account_id,
        reason,
        req.end_revision,
        token,
    )
    .await
    {
        return Err(finish_refused(tx, e).await?);
    }
    tx.commit().await?;
    tracing::info!(%session, reason, "left");
    Ok(StatusCode::NO_CONTENT)
}

/// `?game=`.
#[derive(Deserialize)]
struct GameQuery {
    game: Option<String>,
}

/// `GET /gate/v1/queue?game=` → 200, public, `Cache-Control: public, max-age=10`.
async fn queue(
    State(state): State<AppState>,
    query: Result<Query<GameQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let game = query
        .ok()
        .and_then(|q| q.0.game)
        .filter(|g| (2..=16).contains(&g.len()) && g.bytes().all(|b| b.is_ascii_alphanumeric()))
        .ok_or_else(|| ApiError::invalid("game", "a game code, e.g. ?game=fh2"))?;
    let summary = gate::queue_summary(&state.pool, &game)
        .await?
        .ok_or_else(|| ApiError::invalid("game", "a known game"))?;
    let body = QueueSummary {
        game: summary.game,
        pools: summary
            .pools
            .into_iter()
            .map(|p| PoolQueue {
                name: p.name,
                tier: p.tier,
                active: p.active,
                queued: p.queued,
                capacity: p.capacity,
                accepting: p.accepting,
                eta_seconds: p.eta_seconds,
            })
            .collect(),
        online: OnlineQueue {
            capacity: summary.online.capacity,
            active: summary.online.active,
            queued: summary.online.queued,
        },
        maintenance_until: summary.maintenance_until.map(views::at),
        generated_at: views::at(summary.now),
    };
    let mut response = axum::Json(body).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=10"),
    );
    Ok(response)
}

/// `GET /gate/v1/internal/pools` → every pool of every game with its live counts.
async fn internal_pools(
    internal: InternalAccess,
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
) -> Result<axum::Json<Pools>, ApiError> {
    if internal != InternalAccess::Open {
        return Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "the internal surface is closed on this deployment",
        ));
    }
    let pools = admission::occupancy(&state.pool)
        .await?
        .into_iter()
        .map(|o| PoolStatus {
            instance_capacity: platform.instance_capacity(&o.name),
            game: o.game,
            name: o.name,
            tier: o.tier,
            capacity: o.capacity,
            queue_limit: o.queue_limit,
            accepting: o.accepting,
            active: o.active,
            queued: o.queued,
            game_endpoint: o.game_endpoint,
        })
        .collect();
    Ok(axum::Json(Pools { pools }))
}
