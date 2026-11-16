//! fi-gate: the gate of platform v1 (ADR 0013, ADR 0015; docs/contracts/GATE_API.md). It owns
//! the only table of who is in or waiting for a game (`play_session`), issues the game access
//! ticket after admission, keeps slots alive by heartbeat, and lets players leave; the hourly
//! sweeper is `fi-cli gate-sweep`. It mounts fi-api's `/launcher/v1/sessions*` routes as aliases
//! with the old field names (register CR-S11) and serves `/gate/v1` ([`routes`]).
//!
//! Health (`/health/live`, `/health/ready`: database round trip and every embedded migration
//! applied) and `/meta` come from fi-runtime. The origin lock (`FI_EDGE_SECRET`, ADR 0903 §1) and
//! the client address come from fi-api's `origin_locked`. Nothing mutable lives across requests:
//! every decision is a Postgres transaction (services/AGENTS.md); the per-instance token buckets
//! of [`routes::Limits`] are derived state.

pub mod config;
pub mod routes;

use axum::{Extension, Router};
use fi_runtime::{ServiceInfo, http};
use fi_surfaces::state::{AppState, Platform};
use std::sync::Arc;

/// The gate's router over `state` (whose identity configuration is `config.identity`).
pub fn router(info: ServiceInfo, state: AppState, config: &config::GateConfig) -> Router {
    let control_pool = state.pool.clone();
    let pool = state.pool.clone();
    let ready = move || {
        let pool = pool.clone();
        async move {
            fi_db::ping(&pool)
                .await
                .map_err(|_| "database unreachable".to_owned())?;
            fi_db::migrate::check_applied(&pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    };
    let routes = with_extensions(
        http::health_router(ready)
            .merge(http::meta_router(info))
            .merge(api(state, config)),
        config,
    );
    fi_surfaces::extract::origin_locked(
        fi_surfaces::online_control::protect(routes, control_pool),
        config.edge.clone(),
    )
}

fn api(state: AppState, config: &config::GateConfig) -> Router {
    Router::new()
        .merge(fi_surfaces::launcher::sessions::router())
        .merge(fi_surfaces::content::router())
        .merge(routes::router(config.internal))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            routes::rate_limit,
        ))
        .with_state(state)
}

fn with_extensions(routes: Router, config: &config::GateConfig) -> Router {
    let platform: Platform = Arc::new(config.platform.clone());
    routes
        .layer(Extension::<Platform>(platform))
        .layer(Extension(Arc::new(routes::Limits::default())))
}

/// `/gate/v1` and the `/launcher/v1/sessions*` aliases with the gate's limits and extensions,
/// behind `online`, for a process that mounts several surfaces (ADR 0023 §1, `fi-infinity`): no
/// health, meta or origin lock.
pub fn surface(
    state: AppState,
    config: &config::GateConfig,
    online: &fi_surfaces::online_control::OnlineGuard,
) -> Router {
    online.guard(with_extensions(api(state, config), config))
}

/// `/meta` labels: routing, lease, free liveness window, ticket lifetime, internal access.
pub fn service_info(config: &config::GateConfig) -> ServiceInfo {
    let routing = &config.platform.routing;
    ServiceInfo::from_env("fi-gate", env!("CARGO_PKG_VERSION"))
        .with_label("surfaces", "gate,launcher-sessions")
        .with_label(
            "routing",
            format!(
                "vip={},free={}",
                routing.vip.as_str(),
                routing.free.as_str()
            ),
        )
        .with_label("lease_ttl_ms", config.platform.lease_ttl_ms.to_string())
        .with_label(
            "free_liveness_ms",
            config.platform.free_liveness_ms.to_string(),
        )
        .with_label("ticket_ttl_secs", fi_auth::session::TTL_SECS.to_string())
        .with_label(
            "internal",
            match config.internal {
                config::InternalAccess::Open => "open",
                config::InternalAccess::Off => "off",
            },
        )
        .with_label("instance", config.platform.instance_id.clone())
        .with_label("edge_lock", config.edge.is_some().to_string())
}
