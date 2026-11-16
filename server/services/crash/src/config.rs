//! Production identity verification and the shared public-origin lock.
use fi_runtime::{config::ConfigError, http::EdgeSecret};
use fi_surfaces::config::{Env, IdentityConfig, IdentityNeeds};
use std::sync::Arc;

/// Metadata intake verifies platform access tokens; it never signs tokens or accepts ID tokens.
pub struct Config {
    /// Core's public access-token verification keys.
    pub identity: Arc<IdentityConfig>,
    /// Cloudflare's origin lock, required in production.
    pub edge: Option<Arc<EdgeSecret>>,
}

impl Config {
    /// Read the same production checks used by the other public HTTP services.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        Ok(Self {
            identity: Arc::new(IdentityConfig::from_env_for(
                env,
                IdentityNeeds {
                    launch_tickets: false,
                    ..IdentityNeeds::GATE
                },
            )?),
            edge: fi_surfaces::config::edge_secret(env)?,
        })
    }
}
