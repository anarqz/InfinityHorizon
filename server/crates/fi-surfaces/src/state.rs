//! What every handler shares: the pool, the identity configuration and the platform settings.
//! Nothing mutable across requests lives here (services/AGENTS.md): every decision reads Postgres.
//! The one exception is derived, per-instance and bounded: the game plane's liveness cache
//! (`PlatformConfig::liveness`, ADR 0008 §4 amendment), whose loss costs one read.
//!
//! [`AppState`] is the router state; the platform settings (pool routing, the lease TTL, the
//! session-token keys, the saves bucket, the instance id and the game deployment's pool) travel as
//! a request extension ([`Platform`], set by [`crate::app::router`] from the deployment's
//! [`crate::app::Config`]), so `AppState` keeps the shape `main.rs` builds.

use crate::config::{IdentityConfig, PlatformConfig};
use crate::errors::ApiError;
use fi_db::PgPool;
use fi_protocol::ErrorCode;
use std::sync::Arc;

/// Handler state.
#[derive(Clone)]
pub struct AppState {
    /// A `fi_runtime` login.
    pub pool: PgPool,
    /// Present when the launcher or portal surface is served.
    pub identity: Option<Arc<IdentityConfig>>,
}

impl AppState {
    /// The identity configuration (routes that need it are only mounted with it).
    pub fn id(&self) -> &IdentityConfig {
        self.identity
            .as_deref()
            .expect("identity routes are mounted only with an identity configuration")
    }
}

/// The platform settings of this deployment (an `axum::Extension` on every route).
pub type Platform = Arc<PlatformConfig>;

/// The session-token key ring (loaded whenever the launcher or game surface is served).
pub fn session_keys(p: &PlatformConfig) -> Result<&fi_auth::keys::SigningKeys, ApiError> {
    p.session_keys.as_ref().ok_or_else(|| {
        tracing::error!("session-token keys are not configured on this deployment");
        ApiError::new(ErrorCode::ServiceUnavailable, "service unavailable")
    })
}

/// Unix seconds by this instance's clock: only for stateless token checks (ID tokens, access
/// tokens, session tokens); every stored decision uses the database clock.
pub fn now_secs() -> i64 {
    fi_domain::UnixMs::now_system().millis().div_euclid(1_000)
}
