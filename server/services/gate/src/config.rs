//! The gate's configuration, read once at startup (docs/contracts/GATE_API.md, ADR 0015):
//!
//! - the identity settings of fi-api for the gate (`IdentityNeeds::GATE`): the public keys of
//!   core's access-token ring `FI_PLATFORM_VERIFY_KEYS` (core's private ring is refused), and the
//!   launch-ticket ring `FI_LAUNCH_TICKET_SIGNING_KEYS`; no Identity Platform, no pepper;
//! - the game access ticket's ring `FI_TOKEN_SIGNING_KEYS` (token-signing-key: the gate is its only
//!   reader, ADR 0014). Locally, without `FI_PLATFORM_VERIFY_KEYS`, the same ring also verifies
//!   access tokens (compose runs one ring);
//! - `FI_EDGE_SECRET`: the origin lock (ADR 0903 §1), required in prod;
//! - the platform settings of fi-api (`FI_POOLS_JSON`, `FI_LEASE_TTL_MS`, `FI_FREE_LIVENESS_MS`,
//!   `FI_INSTANCE_ID`), with the gate's routing: `FI_POOL_ROUTING`, default
//!   `vip=vip,free=matchmaking` (free plays on the game's matchmaking pool, CR-L16).
//!   `FI_FREE_LIVENESS_MS` (OD-40, GATE_API.md §4) is the free sessions' slot: default and most
//!   600000 (the ticket's lifetime), least 30000, below the default only outside prod; the
//!   launcher calls every 80% of it. The gate records it per session (`lease_ttl_ms`), so the
//!   sweeper and the game surfaces need no setting of their own;
//! - `FI_GATE_INTERNAL`: `open` (default locally) serves `/gate/v1/internal/*` on the private
//!   network; `off` (default in prod) answers `AUTH.FORBIDDEN` until the OIDC check exists.
//!
//! A lease above the 60-minute ceiling is refused (the owner's rule: nothing lives 60 minutes
//! without a signal), as are the local knobs in prod (fi-api's checks).

use fi_auth::keys::SigningKeys;
use fi_db::admission::SILENCE_CEILING_MS;
use fi_domain::{PoolName, PoolRouting};
use fi_runtime::config::ConfigError;
use fi_runtime::http::EdgeSecret;
use fi_surfaces::config::{Env, IdentityConfig, IdentityNeeds, PlatformConfig, is_prod};
use std::sync::Arc;

/// Who may call `/gate/v1/internal/*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InternalAccess {
    /// Anyone on the network (local compose only).
    Open,
    /// Nobody yet (production until the OIDC check is built).
    Off,
}

/// Everything the gate reads at startup.
#[derive(Clone, Debug)]
pub struct GateConfig {
    /// Access tokens, launch tickets, Identity Platform.
    pub identity: Arc<IdentityConfig>,
    /// Routing, pools, the lease, the instance id and the ticket ring.
    pub platform: PlatformConfig,
    /// `FI_GATE_INTERNAL`.
    pub internal: InternalAccess,
    /// The origin lock.
    pub edge: Option<Arc<EdgeSecret>>,
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

/// The gate's default routing: `vip` to the game's `vip` pool, free to its matchmaking pool.
pub fn default_routing() -> PoolRouting {
    PoolRouting {
        vip: PoolName::vip(),
        free: PoolName::new(fi_db::gate::MATCHMAKING_POOL).expect("a valid pool name"),
    }
}

impl GateConfig {
    /// Reads and checks the gate's settings.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        let identity = IdentityConfig::from_env_for(env, IdentityNeeds::GATE)?;
        Self::from_env_with(env, Arc::new(identity))
    }

    /// The same settings over an identity configuration read once by a process that mounts
    /// several surfaces (ADR 0023 §4): its access ring may sign, which the gate alone refuses.
    pub fn from_env_with(
        env: &dyn Env,
        identity: Arc<IdentityConfig>,
    ) -> Result<Self, ConfigError> {
        let prod = is_prod(env);
        let mut platform = PlatformConfig::from_env(env, None, false, false)?;
        if env.get("FI_POOL_ROUTING").is_none() {
            platform.routing = default_routing();
        }
        if i64::from(platform.lease_ttl_ms) > SILENCE_CEILING_MS {
            return Err(invalid(
                "FI_LEASE_TTL_MS",
                "at most 3600000: nothing holds a slot 60 minutes without a signal",
            ));
        }
        if platform.local_admin {
            return Err(invalid(
                "FI_LOCAL_ADMIN",
                "the gate has no local administration",
            ));
        }
        // The game access ticket's ring (ADR 0014: the gate mints with token-signing-key; game
        // surfaces hold its public part, FI_TOKEN_VERIFY_KEYS).
        let text = env
            .get("FI_TOKEN_SIGNING_KEYS")
            .ok_or(ConfigError::Missing("FI_TOKEN_SIGNING_KEYS"))?;
        let ring = SigningKeys::parse("FI_TOKEN_SIGNING_KEYS", &text)
            .map_err(|e| invalid("FI_TOKEN_SIGNING_KEYS", e.reason))?;
        if prod && ring.has_local_dev_key() {
            return Err(invalid(
                "FI_TOKEN_SIGNING_KEYS",
                "a well-known local development key is refused in prod",
            ));
        }
        platform.session_keys = Some(ring);
        let edge = fi_surfaces::config::edge_secret(env)?;
        let internal = match (env.get("FI_GATE_INTERNAL").as_deref().map(str::trim), prod) {
            (None, true) | (Some("off"), _) => InternalAccess::Off,
            (None, false) | (Some("open"), false) => InternalAccess::Open,
            (Some("open"), true) => {
                return Err(invalid(
                    "FI_GATE_INTERNAL",
                    "an open internal surface is refused when APP_ENV or FI_ENV is prod",
                ));
            }
            (Some("oidc"), _) => {
                return Err(invalid(
                    "FI_GATE_INTERNAL",
                    "oidc is not implemented yet (platform v1 follow-up): use off",
                ));
            }
            (Some(_), _) => return Err(invalid("FI_GATE_INTERNAL", "open or off")),
        };
        Ok(Self {
            identity,
            platform,
            internal,
            edge,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_TOKEN: &str = "at-local-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=";
    const LOCAL_TICKET: &str = "lt-local-1:8BxGVNw9Cyi6bbk5aWzYhSbdgW8CRkM125qYUron8uI=";
    const LOCAL_PEPPER: &str = "1:84yC96SRqM8E+D3MCtARPCjLJ7eiFc/pGvdtLu2TaHQ=";

    fn local() -> Vec<(&'static str, String)> {
        vec![
            ("APP_ENV", "local".into()),
            ("GCP_PROJECT_ID", "demo-forza-infinity".into()),
            ("FIREBASE_AUTH_EMULATOR_HOST", "127.0.0.1:9399".into()),
            ("FI_IDP_PROJECT_ID", "demo-forza-infinity".into()),
            ("FI_TOKEN_SIGNING_KEYS", LOCAL_TOKEN.into()),
            ("FI_LAUNCH_TICKET_SIGNING_KEYS", LOCAL_TICKET.into()),
            ("FI_SITE_KEY_PEPPER", LOCAL_PEPPER.into()),
        ]
    }

    fn with(
        mut env: Vec<(&'static str, String)>,
        k: &'static str,
        v: &str,
    ) -> Vec<(&'static str, String)> {
        env.retain(|(n, _)| *n != k);
        env.push((k, v.to_owned()));
        env
    }

    #[test]
    fn local_defaults_route_free_to_matchmaking_and_open_internal() {
        let c = GateConfig::from_env(&local()).unwrap();
        assert_eq!(c.platform.routing, default_routing());
        assert_eq!(c.platform.routing.free.as_str(), "matchmaking");
        assert_eq!(c.platform.lease_ttl_ms, 300_000);
        assert_eq!(c.platform.free_liveness_ms, 600_000);
        assert_eq!(c.internal, InternalAccess::Open);
        assert!(c.platform.session_keys.is_some());
        let c =
            GateConfig::from_env(&with(local(), "FI_POOL_ROUTING", "vip=vip,free=free")).unwrap();
        assert_eq!(c.platform.routing.free.as_str(), "free");
    }

    #[test]
    fn a_lease_above_the_hour_and_open_internals_in_prod_are_refused() {
        let refused =
            |env: Vec<(&'static str, String)>, name: &str| match GateConfig::from_env(&env) {
                Err(ConfigError::Invalid { name: n, .. }) | Err(ConfigError::Missing(n)) => {
                    assert_eq!(n, name)
                }
                Ok(_) => panic!("{name} must be refused"),
            };
        refused(
            with(local(), "FI_LEASE_TTL_MS", "3600001"),
            "FI_LEASE_TTL_MS",
        );
        assert!(GateConfig::from_env(&with(local(), "FI_LEASE_TTL_MS", "3600000")).is_ok());
        // The free liveness window (OD-40): 30 s to the ticket's 10 minutes.
        let c = GateConfig::from_env(&with(local(), "FI_FREE_LIVENESS_MS", "90000")).unwrap();
        assert_eq!(c.platform.slot_ttl().for_tier("free"), 90_000);
        assert_eq!(c.platform.slot_ttl().for_tier("vip"), 300_000);
        for bad in ["29999", "600001", "-1", "1e5"] {
            refused(
                with(local(), "FI_FREE_LIVENESS_MS", bad),
                "FI_FREE_LIVENESS_MS",
            );
        }
        refused(
            with(local(), "FI_GATE_INTERNAL", "oidc"),
            "FI_GATE_INTERNAL",
        );
        refused(with(local(), "FI_GATE_INTERNAL", "yes"), "FI_GATE_INTERNAL");
        refused(with(local(), "FI_LOCAL_ADMIN", "true"), "FI_LOCAL_ADMIN");
        let off = GateConfig::from_env(&with(local(), "FI_GATE_INTERNAL", "off")).unwrap();
        assert_eq!(off.internal, InternalAccess::Off);
        // In prod the local emulator, keys and an open internal surface never start.
        let prod = with(local(), "APP_ENV", "prod");
        assert!(GateConfig::from_env(&prod).is_err());
    }
}
