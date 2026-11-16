//! Play sessions (SAVES_API.md §3): create with admission, poll while queued, heartbeat with
//! resume and re-admission, token rotation, end, installed. Launch tickets (`fi_auth::ticket`)
//! and game access tickets (`fi_auth::session`, the session token of this surface) are minted
//! after the admission transaction commits. Built by workstream A of the local environment v0
//! (docs/server/LOCAL_ENV.md §9); from platform v1 these routes are the gate's aliases
//! (GATE_API.md, register CR-S11) with the old field names, and the helpers below are what
//! `services/gate` builds `/gate/v1` with.
//!
//! Every decision is one transaction of `fi_db::admission` (pool lock, then the session row, then
//! the save head, then the session rows and the read models), so any instance answers the same and a
//! crash between two requests loses nothing. `Idempotency-Key` is the session id: a retried create
//! answers the same session (with fresh credentials: the first answer may never have arrived).
//! The game's maintenance switch refuses new sessions here as on `/gate/v1`.
//!
//! Cloud eligibility (OD-52, SAVES_API.md §3.1): these bodies always carry a lease, so a create
//! admits only a profile that keeps a save line (a Supporter's Hardcore profile): the admission
//! ([`SaveLinePolicy::Always`]) refuses any other in its transaction, before anything is written
//! (403 `SAVES.NOT_ELIGIBLE`; a retired line 404 `SAVES.PROFILE_NOT_FOUND`); a queued session
//! admitted on a poll without a lease (the account lapsed while it waited) is rolled back and
//! refused the same.

use super::saves::{head_body, not_eligible, session_profile_of};
use crate::errors::ApiError;
use crate::extract::{Body, IdempotencyKey, LauncherAuth};
use crate::state::{AppState, Platform, session_keys};
use crate::views;
use axum::Router;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fi_auth::session::{self, Grant};
use fi_auth::ticket::{self, TicketClaims};
use fi_db::admission::{self, Admitted, Outcome, SaveLinePolicy};
use fi_db::{PgSaveStore, gate};
use fi_domain::{PlaySessionId, Tier, UnixMs};
use fi_protocol::ErrorCode;
use fi_protocol::game::SessionTokenGrant;
use fi_protocol::routes::launcher as r;
use fi_protocol::saves::{
    EndReason, EndSession, GamePlane, Heartbeat, HeartbeatResponse, Installed, Lease, PlaySession,
    PlaySessionCreate, Pool, Queue,
};
use fi_saves::{FenceToken, SaveStore};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

/// The play-session routes of the launcher surface.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::SESSIONS, post(create))
        .route(r::SESSION, get(poll))
        .route(r::SESSION_HEARTBEAT, post(heartbeat))
        .route(r::SESSION_TOKEN, post(token))
        .route(r::SESSION_END, post(end))
        .route(r::SESSION_INSTALLED, post(installed))
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn digest32(hex: &str) -> Option<[u8; 32]> {
    let d: fi_saves::Digest = hex.parse().ok()?;
    Some(d.0)
}

/// A session id from a path or body (`REQUEST.INVALID {field: sessionId}`).
pub fn session_id(text: &str) -> Result<PlaySessionId, ApiError> {
    Uuid::parse_str(text.trim())
        .ok()
        .and_then(PlaySessionId::from_uuid)
        .ok_or_else(|| ApiError::invalid("sessionId", "a UUID session id"))
}

/// A fencing token from a body (`REQUEST.INVALID {field: fenceToken}`).
pub fn fence(value: u64) -> Result<FenceToken, ApiError> {
    FenceToken::new(value).ok_or_else(|| ApiError::invalid("fenceToken", "a fence token is >= 1"))
}

/// Unix seconds of a database instant.
pub fn secs(t: UnixMs) -> i64 {
    t.millis().div_euclid(1_000)
}

/// The lease as the launcher sees it (`heartbeatEveryMs` = [`heartbeat_every_ms`]: a fifth of a
/// Supporter's lease); `None` without a save line.
pub fn lease_body(a: &Admitted) -> Option<Lease> {
    a.lease.as_ref().map(|l| Lease {
        fence_token: l.token.get(),
        ttl_ms: l.ttl_ms,
        heartbeat_every_ms: admission::heartbeat_every_ms(&a.pool_tier, true, l.ttl_ms),
        expires_at: views::at(l.expires_at),
    })
}

/// The alias bodies always carry a lease; a gate session without a save line has none.
fn required_lease(a: &Admitted) -> Result<Lease, ApiError> {
    lease_body(a).ok_or_else(|| {
        ApiError::new(
            ErrorCode::SavesLeaseLost,
            "this session has no save line: use /gate/v1",
        )
    })
}

/// When to heartbeat (GATE_API.md §4): a fifth of a Supporter's lease (60 s for the contract's 5
/// minutes), 80% of a free session's liveness window (OD-40: 480 s for 10 minutes).
pub fn heartbeat_every_ms(a: &Admitted) -> u32 {
    a.heartbeat_every_ms()
}

fn tier_str(t: Tier) -> &'static str {
    t.as_str()
}

/// A minted game access ticket.
#[derive(Debug)]
pub struct MintedTicket {
    /// The token (a bearer secret for game surfaces).
    pub token: String,
    /// Its `exp`, Unix seconds.
    pub expires_at: i64,
}

/// The latest `exp` a game access ticket of `a` may carry (Unix seconds): the session's game-plane
/// end and, for a Supporter's ticket, never past the account's `access_until` (ACCOUNTS.md §7): a
/// ticket minted before a lapse stops at it, so no Supporter claim outlives the access. `tier` is
/// `vip` only while `access_until` is ahead, so the cap never mints a ticket born expired.
pub fn ticket_until(a: &Admitted) -> i64 {
    let until = secs(a.game_until);
    match (a.tier, a.access_until) {
        (Tier::Vip, Some(access)) => until.min(secs(access)),
        _ => until,
    }
}

/// `kid` and `exp` of the ticket [`mint_ticket`] mints for `a` (same clock, same bound), to
/// record in `play_session` before the transaction commits.
pub fn ticket_record(p: &Platform, a: &Admitted) -> Result<(String, UnixMs), ApiError> {
    let kid = session_keys(p)?.current().0.to_owned();
    let exp = session::expiry(secs(a.now), ticket_until(a));
    Ok((kid, UnixMs::from_millis(exp.saturating_mul(1_000))))
}

/// Records the ticket about to be minted for `a` on the caller's connection.
pub async fn record_ticket_in(
    conn: &mut PgConnection,
    p: &Platform,
    a: &Admitted,
) -> Result<(), ApiError> {
    let (kid, exp) = ticket_record(p, a)?;
    gate::record_ticket_in(conn, a.session_id, &kid, exp).await?;
    Ok(())
}

/// The game access ticket for `a` (10 minutes, never past [`ticket_until`]), on the database clock.
pub fn mint_ticket(p: &Platform, a: &Admitted) -> Result<MintedTicket, ApiError> {
    let keys = session_keys(p)?;
    let issued = session::issue_for_game(
        keys,
        &Grant {
            session_id: &a.session_id.to_string(),
            profile_id: &a.profile_id.to_string(),
            account_id: &a.account_id.to_string(),
            xuid: a.xuid,
            pool: &a.pool,
            tier: tier_str(a.tier),
            mode: &a.mode,
            car_db: &a.ticket_car_db(),
            line: a.save_line,
            game_until: ticket_until(a),
        },
        &a.game,
        secs(a.now),
    );
    Ok(MintedTicket {
        token: issued.token.into_inner(),
        expires_at: issued.expires_at,
    })
}

fn session_token(p: &Platform, a: &Admitted) -> Result<(String, String), ApiError> {
    let t = mint_ticket(p, a)?;
    Ok((t.token, views::at_secs(t.expires_at)))
}

/// The launch ticket of an admitted session (ACCOUNTS.md §4.2): the pool's endpoint is `server`.
pub fn launch_ticket(
    state: &AppState,
    a: &Admitted,
) -> Result<fi_protocol::saves::LaunchTicket, ApiError> {
    let iat = secs(a.now).max(0) as u64;
    let exp =
        (iat + ticket::MAX_LIFETIME_SECS).min((secs(a.game_until).max(0) as u64).max(iat + 1));
    let claims = TicketClaims {
        v: 1,
        kid: String::new(),
        sid: a.session_id.to_string(),
        account_id: a.account_id.to_string(),
        profile_id: a.profile_id.to_string(),
        game: a.game.clone(),
        xuid: a.xuid,
        gamertag: a.gamertag.clone(),
        mode: a.mode.clone(),
        server: a.game_endpoint.clone(),
        tier: tier_str(a.tier).to_owned(),
        pool: a.pool.clone(),
        nonce: a.nonce.clone().unwrap_or_default(),
        iat,
        exp,
    };
    ticket::sign(state.id().ticket_keys()?, claims)
        .map(|t| t.to_wire())
        .map_err(|_| {
            tracing::error!(session = %a.session_id, "launch ticket claims out of bounds");
            ApiError::unavailable()
        })
}

/// `pool` of a body: the pool's name, the account's tier, the pool's endpoint.
pub fn pool_body(a: &Admitted) -> Pool {
    Pool {
        name: a.pool.clone(),
        tier: tier_str(a.tier).to_owned(),
        server: a.game_endpoint.clone(),
    }
}

/// `gamePlane` of a heartbeat.
pub fn game_plane(a: &Admitted) -> GamePlane {
    GamePlane {
        open: a.now < a.game_until,
        until: views::at(a.game_until),
        reason: Some(a.game_plane_reason().to_owned()),
    }
}

async fn active_body(
    state: &AppState,
    p: &Platform,
    a: &Admitted,
) -> Result<PlaySession, ApiError> {
    let lease = required_lease(a)?;
    let (session_token, session_token_expires_at) = session_token(p, a)?;
    let head = PgSaveStore::new(state.pool.clone())
        .head(a.profile_id)
        .await?;
    Ok(PlaySession::Active {
        session_id: a.session_id.to_string(),
        ticket: launch_ticket(state, a)?,
        session_token,
        session_token_expires_at,
        lease,
        pool: pool_body(a),
        save: head_body(&state.pool, p, &a.game, a.account_id, &head).await?,
    })
}

fn queued_body(session: PlaySessionId, position: u32) -> PlaySession {
    PlaySession::Queued {
        session_id: session.to_string(),
        queue: Queue {
            position,
            eta_seconds: None,
            retry_after_seconds: admission::RETRY_AFTER_SECS,
        },
    }
}

async fn outcome_response(
    state: &AppState,
    p: &Platform,
    outcome: Outcome,
    active: StatusCode,
    queued: StatusCode,
) -> Result<Response, ApiError> {
    Ok(match outcome {
        Outcome::Active(a) => {
            (active, axum::Json(active_body(state, p, &a).await?)).into_response()
        }
        Outcome::Queued {
            session_id,
            position,
        } => (queued, axum::Json(queued_body(session_id, position))).into_response(),
    })
}

/// A create body that passed the checks every admission makes before reading the database.
#[derive(Clone, Debug)]
pub struct CheckedCreate {
    /// The session id (equal to the `Idempotency-Key`).
    pub session: PlaySessionId,
    /// The declared car database.
    pub car_db: Option<[u8; 32]>,
    /// The handoff nonce, lower case.
    pub nonce: String,
}

/// The SHA-256 of zero bytes: the `hookSha256` of an install without a hook (CR-L20) and of a
/// dedicated session (OD-109).
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The checks of SAVES_API.md §3.1 / GATE_API.md §2 that need no database: the key is the
/// session id, the nonce and digests are hex, the launcher version is bounded, the car database
/// is allowed online (`GAME.CAR_DB_REFUSED`; not for a dedicated session, OD-109, whose hook
/// digest is [`EMPTY_SHA256`]), and the account is not pending deletion.
pub fn check_create(
    state: &AppState,
    auth: &LauncherAuth,
    key: &IdempotencyKey,
    req: &PlaySessionCreate,
    dedicated: bool,
) -> Result<CheckedCreate, ApiError> {
    let session = session_id(&req.session_id)?;
    if session.as_uuid() != key.0 {
        return Err(ApiError::invalid(
            "Idempotency-Key",
            "the Idempotency-Key of a session create is its sessionId",
        ));
    }
    if !is_hex(&req.nonce, 32) {
        return Err(ApiError::invalid("nonce", "32 hex digits"));
    }
    if !is_hex(&req.hook_sha256, 64) {
        return Err(ApiError::invalid("hookSha256", "a SHA-256 in hex"));
    }
    if fi_domain::version::LauncherVersion::parse(&req.launcher_version).is_err() {
        return Err(ApiError::invalid(
            "launcherVersion",
            "strict SemVer, 1 to 64 ASCII bytes",
        ));
    }
    let car_db = match req.car_db_sha256.as_str() {
        "" => None,
        hex => Some(
            digest32(hex).ok_or_else(|| ApiError::invalid("carDbSha256", "a SHA-256 in hex"))?,
        ),
    };
    // OD-109: a dedicated session runs no game: no content scan, no car database, and the hook
    // digest of an install without one (the SHA-256 of zero bytes, CR-L20).
    if dedicated && !req.hook_sha256.eq_ignore_ascii_case(EMPTY_SHA256) {
        return Err(ApiError::invalid(
            "hookSha256",
            "a dedicated session declares the SHA-256 of zero bytes",
        ));
    }
    // OD-113: only a declared car database is held to the allowed list; a session that declares
    // none (every session but a Supporter's Hardcore one, which the content check binds to the
    // original car database) is not refused for it.
    let allowed = &state.id().info.allowed_car_db;
    if !dedicated
        && !allowed.is_empty()
        && car_db.is_some_and(|d| !allowed.iter().any(|a| digest32(a) == Some(d)))
    {
        return Err(ApiError::new(
            ErrorCode::GameCarDbRefused,
            "this car database is not allowed online",
        ));
    }
    if auth.account.status == "deletion_pending" {
        return Err(ApiError::new(
            ErrorCode::AccountsDeletionPending,
            "the account is pending deletion",
        ));
    }
    Ok(CheckedCreate {
        session,
        car_db,
        nonce: req.nonce.to_ascii_lowercase(),
    })
}

/// `SERVICE.MAINTENANCE {until}` with `Retry-After` (at most an hour; the launcher polls).
pub fn maintenance(until: UnixMs, now: UnixMs) -> ApiError {
    let wait = until.since(now).div_euclid(1_000).clamp(1, 3_600) as u64;
    ApiError::new(
        ErrorCode::ServiceMaintenance,
        "the game is in maintenance: nobody new is admitted",
    )
    .with_details(json!({ "until": views::at(until) }))
    .retry_after(wait)
}

/// `POST /launcher/v1/sessions` → 201 active, 202 queued (SAVES_API.md §3.1).
async fn create(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    key: IdempotencyKey,
    Body(req): Body<PlaySessionCreate>,
) -> Result<Response, ApiError> {
    // OD-109: `dedicated` is /gate/v1's alone; this body refuses it as an unknown field.
    let checked = check_create(&state, &auth, &key, &req, false)?;
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
    .map_err(crate::launcher_policy::error)?;
    crate::content::create_guard_in(&mut tx, &platform, &auth, &req).await?;
    let device = admission::device_of_family_in(&mut tx, auth.account_id, auth.family_id)
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::AuthTokenInvalid, "invalid access token"))?;
    // The gate's admission (game, maintenance, replay); this body always carries a lease, so a
    // profile that keeps no save line (OD-52) is refused there: it uses /gate/v1.
    let admitted = gate::admit_in(
        &mut tx,
        &gate::AdmitRequest {
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
            save_line: SaveLinePolicy::Always,
            dedicated: false,
            // OD-112 follow-up: `modded` is /gate/v1's alone (this body refuses it).
            modded: false,
        },
    )
    .await;
    let outcome = match admitted {
        Ok(o) => o,
        Err(e) => return Err(finish_refused(tx, e).await?),
    };
    if let Some(refusal) = leaseless(&mut tx, &outcome, auth.account_id).await? {
        tx.rollback().await?;
        return Err(refusal);
    }
    if let Outcome::Active(a) = &outcome {
        record_ticket_in(&mut tx, &platform, a).await?;
    }
    tx.commit().await?;
    if let Outcome::Active(a) = &outcome {
        tracing::info!(session = %a.session_id, pool = %a.pool, fence = a.lease.as_ref().map(|l| l.token.get()), "play session admitted");
    }
    outcome_response(
        &state,
        &platform,
        outcome,
        StatusCode::CREATED,
        StatusCode::ACCEPTED,
    )
    .await
}

/// `GET /launcher/v1/sessions/{id}` → 200 with either body (SAVES_API.md §3.1).
async fn poll(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let session = session_id(&id)?;
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
    .map_err(crate::launcher_policy::error)?;
    crate::content::session_guard_in(&mut tx, &platform, &auth, session.as_uuid()).await?;
    // A poll that dropped an unpolled session out of the queue commits that, and answers it.
    let outcome = match gate::poll_in(&mut tx, session, auth.account_id, platform.slot_ttl()).await
    {
        Ok(outcome) => outcome,
        Err(e) => return Err(finish_refused(tx, e).await?),
    };
    // An admission on this poll without a lease is rolled back: the session keeps its place
    // and leaves the queue unpolled (SAVES_API.md §3.1).
    if let Some(refusal) = leaseless(&mut tx, &outcome, auth.account_id).await? {
        tx.rollback().await?;
        return Err(refusal);
    }
    if let Outcome::Active(a) = &outcome {
        record_ticket_in(&mut tx, &platform, a).await?;
    }
    tx.commit().await?;
    outcome_response(&state, &platform, outcome, StatusCode::OK, StatusCode::OK).await
}

/// The refusal of an alias admission that came out without a lease (OD-52: these bodies always
/// carry one): `SAVES.NOT_ELIGIBLE` with the tier and mode of that decision.
async fn leaseless(
    conn: &mut PgConnection,
    outcome: &Outcome,
    account: Uuid,
) -> Result<Option<ApiError>, ApiError> {
    let Outcome::Active(a) = outcome else {
        return Ok(None);
    };
    if a.lease.is_some() {
        return Ok(None);
    }
    let access = fi_db::accounts::access_in(conn, account).await?;
    Ok(Some(not_eligible(&access, &a.mode)))
}

/// Commits the close a refusal carries (`SESSIONS.ENDED`: a queued session dropped for
/// `queue_left`, a session ended at `max_age`; the refusal is the close) and rolls back anything
/// else; answers the refusal.
pub async fn finish_refused(
    tx: sqlx::Transaction<'_, sqlx::Postgres>,
    e: gate::GateError,
) -> Result<ApiError, ApiError> {
    if matches!(
        e,
        gate::GateError::Admission(admission::AdmissionError::Ended { .. })
    ) {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(gate_error(e))
}

/// The gate's refusals as the contract's codes (GATE_API.md §2, §4).
pub fn gate_error(e: gate::GateError) -> ApiError {
    match e {
        gate::GateError::Admission(e) => e.into(),
        gate::GateError::UnknownGame => ApiError::invalid("game", "a known game"),
        gate::GateError::WrongGame => ApiError::invalid("game", "the profile's game"),
        gate::GateError::Maintenance { until, now } => maintenance(until, now),
        gate::GateError::FenceRequired => ApiError::invalid(
            "fenceToken",
            "a Supporter session heartbeats with its fence token",
        ),
    }
}

/// `POST .../heartbeat` → 200 (SAVES_API.md §3.2).
async fn heartbeat(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    Path(id): Path<String>,
    Body(req): Body<Heartbeat>,
) -> Result<axum::Json<HeartbeatResponse>, ApiError> {
    let session = session_id(&id)?;
    let token = fence(req.fence_token)?;
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    crate::content::session_guard_in(&mut tx, &platform, &auth, session.as_uuid()).await?;
    let result = admission::heartbeat_in(
        &mut tx,
        session,
        auth.account_id,
        token,
        platform.slot_ttl(),
    )
    .await;
    // A heartbeat past max_age closes the session: that close commits with the refusal.
    let a = match result {
        Ok(a) => {
            record_ticket_in(&mut tx, &platform, &a).await?;
            tx.commit().await?;
            a
        }
        Err(e @ admission::AdmissionError::Ended { .. }) => {
            tx.commit().await?;
            return Err(e.into());
        }
        Err(e) => {
            tx.rollback().await?;
            return Err(e.into());
        }
    };
    let (session_token, session_token_expires_at) = session_token(&platform, &a)?;
    Ok(axum::Json(HeartbeatResponse {
        lease: required_lease(&a)?,
        session_token,
        session_token_expires_at,
        game_plane: Some(game_plane(&a)),
    }))
}

/// `POST .../token` → 200 `{sessionToken, sessionTokenExpiresAt}` (SAVES_API.md §3.3).
async fn token(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    auth: LauncherAuth,
    Path(id): Path<String>,
) -> Result<axum::Json<SessionTokenGrant>, ApiError> {
    let session = session_id(&id)?;
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    crate::content::session_guard_in(&mut tx, &platform, &auth, session.as_uuid()).await?;
    let a = admission::token_in(&mut tx, session, auth.account_id).await?;
    record_ticket_in(&mut tx, &platform, &a).await?;
    tx.commit().await?;
    let (session_token, session_token_expires_at) = session_token(&platform, &a)?;
    Ok(axum::Json(SessionTokenGrant {
        session_token,
        session_token_expires_at,
    }))
}

/// `POST .../end` → 204 (SAVES_API.md §3.4). Idempotent.
async fn end(
    State(state): State<AppState>,
    auth: LauncherAuth,
    Path(id): Path<String>,
    Body(req): Body<EndSession>,
) -> Result<StatusCode, ApiError> {
    let session = session_id(&id)?;
    let token = fence(req.fence_token)?;
    let reason = match req.reason {
        EndReason::GameExit => "game_exit",
        EndReason::Player => "player",
    };
    let mut tx = state.pool.begin().await?;
    auth.require_live_in(&mut tx).await?;
    admission::end_in(
        &mut tx,
        session,
        auth.account_id,
        token,
        reason,
        req.end_revision,
    )
    .await?;
    tx.commit().await?;
    tracing::info!(%session, reason, "play session ended");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST .../installed` → 204 (SAVES_API.md §3.5): an audit row.
async fn installed(
    State(state): State<AppState>,
    auth: LauncherAuth,
    Path(id): Path<String>,
    Body(req): Body<Installed>,
) -> Result<StatusCode, ApiError> {
    let session = session_id(&id)?;
    if !is_hex(&req.content_sha256, 64) {
        return Err(ApiError::invalid("contentSha256", "a SHA-256 in hex"));
    }
    let mut conn = state.pool.acquire().await?;
    admission::installed_in(
        &mut conn,
        session,
        auth.account_id,
        req.revision,
        &req.content_sha256.to_ascii_lowercase(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_domain::ProfileId;

    fn admitted(tier: Tier, access_in_s: Option<i64>, game_until_in_s: i64) -> Admitted {
        let now = UnixMs::from_millis(1_800_000_000_000);
        Admitted {
            session_id: PlaySessionId::new(),
            account_id: Uuid::now_v7(),
            profile_id: ProfileId::from_uuid(Uuid::now_v7()).unwrap(),
            xuid: 2_535_000_000_000_001,
            gamertag: "Someone#12345".into(),
            game: "fh2".into(),
            mode: "hardcore".into(),
            pool: "vip".into(),
            pool_tier: "vip".into(),
            game_endpoint: "http://127.0.0.1:1/pools/vip".into(),
            tier,
            access_until: access_in_s.map(|s| now.plus_millis(s * 1_000)),
            car_db: None,
            nonce: None,
            lease: None,
            lease_ttl_ms: 20_000,
            expires_at: now.plus_millis(20_000),
            max_until: now.plus_millis(12 * 3_600_000),
            game_until: now.plus_millis(game_until_in_s * 1_000),
            now,
            dedicated: false,
            save_line: false,
        }
    }

    /// ACCOUNTS.md §7: a Supporter's ticket never outlives the access (the review's scenario A),
    /// whatever the game plane's grace; a free ticket ends with the game plane.
    #[test]
    fn a_supporters_ticket_stops_at_access_until() {
        let a = admitted(Tier::Vip, Some(90), 2 * 3_600);
        assert_eq!(ticket_until(&a), secs(a.now) + 90);
        assert_eq!(
            session::expiry(secs(a.now), ticket_until(&a)),
            secs(a.now) + 90,
            "exp = access_until, not now + 10 min"
        );
        let far = admitted(Tier::Vip, Some(30 * 86_400), 2 * 3_600);
        assert_eq!(
            session::expiry(secs(far.now), ticket_until(&far)),
            secs(far.now) + session::TTL_SECS
        );
        let free = admitted(Tier::Free, Some(-60), 2 * 3_600);
        assert_eq!(ticket_until(&free), secs(free.now) + 2 * 3_600);
        let unbounded = admitted(Tier::Vip, None, 300);
        assert_eq!(ticket_until(&unbounded), secs(unbounded.now) + 300);
    }
}
