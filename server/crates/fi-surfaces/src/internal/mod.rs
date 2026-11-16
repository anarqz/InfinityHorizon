//! `/internal/v1` of the account plane (docs/contracts/GAME_PLATFORM.md §3): pool occupancy for
//! the autoscaler stand-in and, only with `FI_LOCAL_ADMIN=true` (refused in prod), the local
//! entitlement stand-in. OIDC-authenticated in production (Pub/Sub push, Cloud Scheduler); open on
//! the compose network locally. Built by workstream A of the local environment v0.

use crate::errors::ApiError;
use crate::extract::Body;
use crate::state::{AppState, Platform};
use crate::views;
use axum::Router;
use axum::extract::{Extension, State};
use axum::routing::{get, put};
use fi_db::admission;
use fi_domain::UnixMs;
use fi_protocol::ErrorCode;
use fi_protocol::game::{LocalEntitlement, LocalEntitlementResult, PoolStatus, Pools};
use fi_protocol::instant;
use fi_protocol::routes::internal as r;
use uuid::Uuid;

/// The internal surface.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::POOLS, get(pools))
        .route(r::LOCAL_ENTITLEMENTS, put(local_entitlement))
}

/// `GET /internal/v1/pools` → every pool with its live counts (database clock) and the
/// `instanceCapacity` of `FI_POOLS_JSON`; 403 unless `FI_LOCAL_ADMIN` (GAME_PLATFORM.md §3.1:
/// production waits for the OIDC check, CR-S47).
async fn pools(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
) -> Result<axum::Json<Pools>, ApiError> {
    if !platform.local_admin {
        return Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "the internal pool list is local only until its OIDC check exists (FI_LOCAL_ADMIN)",
        ));
    }
    let pools = admission::occupancy(&state.pool)
        .await?
        .into_iter()
        .map(|o| PoolStatus {
            instance_capacity: platform.instance_capacity(&o.name),
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

/// `PUT /internal/v1/local/entitlements` (local only): the stand-in entitlement row, as `fi_app`.
async fn local_entitlement(
    State(state): State<AppState>,
    Extension(platform): Extension<Platform>,
    Body(req): Body<LocalEntitlement>,
) -> Result<axum::Json<LocalEntitlementResult>, ApiError> {
    if !platform.local_admin {
        return Err(ApiError::new(
            ErrorCode::AuthForbidden,
            "local administration is off on this deployment (FI_LOCAL_ADMIN)",
        ));
    }
    let account_id = Uuid::parse_str(req.account_id.trim())
        .map_err(|_| ApiError::invalid("accountId", "a UUID"))?;
    if !matches!(req.status.as_str(), "active" | "grace" | "expired" | "none") {
        return Err(ApiError::invalid(
            "status",
            "active, grace, expired or none",
        ));
    }
    if req.plan.as_deref().is_some_and(|p| p != "comp") {
        return Err(ApiError::invalid("plan", "comp"));
    }
    let access_until = req
        .access_until
        .as_deref()
        .map(|t| {
            instant::parse_unix_micros(t)
                .map(|m| UnixMs::from_millis(m.div_euclid(1_000)))
                .ok_or_else(|| ApiError::invalid("accessUntil", "a wire instant"))
        })
        .transpose()?;
    // The entitlement table refuses an unbounded VIP row (entitlement_vip_is_bounded).
    if access_until.is_none() && matches!(req.status.as_str(), "active" | "grace") {
        return Err(ApiError::invalid(
            "accessUntil",
            "an instant when status is active or grace",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let access = admission::set_local_entitlement_in(
        &mut tx,
        account_id,
        &req.status,
        access_until,
        req.plan.as_deref(),
    )
    .await
    .map_err(|e| match e {
        admission::AdmissionError::NotFound => {
            ApiError::new(ErrorCode::AccountsNotFound, "no such account")
        }
        other => other.into(),
    })?;
    tx.commit().await?;
    tracing::info!(account = %account_id, status = %access.status, vip = access.vip, "local entitlement set");
    Ok(axum::Json(LocalEntitlementResult {
        account_id: account_id.to_string(),
        tier: if access.vip { "vip" } else { "free" }.to_owned(),
        status: access.status,
        access_until: access.access_until.map(views::at),
    }))
}
