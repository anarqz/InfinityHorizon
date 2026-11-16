//! Configuration and router of fi-api.

use crate::config::{PlatformConfig, ProcessEnv};
use crate::state::{AppState, Platform};
use crate::{game, internal, launcher, portal};
use axum::{Extension, Router};
use fi_runtime::config::{self, ConfigError};
use fi_runtime::{ServiceInfo, http};
use std::sync::Arc;

/// A group of routes a deployment serves (ADR 0002 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// `/launcher/v1`: accounts, tokens, play sessions, saves.
    Launcher,
    /// `/portal/v1`: the portal's server-to-server API.
    Portal,
    /// The hook's game traffic for one pool.
    Game,
    /// `/internal/v1`: Pub/Sub push and Cloud Scheduler, OIDC-authenticated.
    Internal,
}

impl Surface {
    const ALL: [Surface; 4] = [
        Surface::Launcher,
        Surface::Portal,
        Surface::Game,
        Surface::Internal,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Surface::Launcher => "launcher",
            Surface::Portal => "portal",
            Surface::Game => "game",
            Surface::Internal => "internal",
        }
    }
}

/// What a deployment serves.
#[derive(Clone, Debug)]
pub struct Config {
    /// `FI_SURFACES`, comma separated.
    pub surfaces: Vec<Surface>,
    /// `FI_POOL`: the pool a game-surface deployment serves; required with `game`, refused without.
    pub pool: Option<String>,
    /// Play sessions, saves, the game plane and the internal surface (local environment v0).
    pub platform: PlatformConfig,
}

impl Config {
    /// Reads `FI_SURFACES`, `FI_POOL` and the platform settings the surfaces need.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::parse(
            &config::required("FI_SURFACES")?,
            config::optional("FI_POOL"),
        )?;
        config.platform = PlatformConfig::from_env(
            &ProcessEnv,
            config.pool.clone(),
            config.serves(Surface::Launcher),
            config.serves(Surface::Game),
        )?;
        Ok(config)
    }

    pub(crate) fn parse(surfaces: &str, pool: Option<String>) -> Result<Self, ConfigError> {
        let mut out = Vec::new();
        for part in surfaces.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let surface = Surface::ALL
                .into_iter()
                .find(|s| s.as_str() == part)
                .ok_or_else(|| ConfigError::Invalid {
                    name: "FI_SURFACES",
                    reason: format!("unknown surface {part:?}"),
                })?;
            if !out.contains(&surface) {
                out.push(surface);
            }
        }
        if out.is_empty() {
            return Err(ConfigError::Invalid {
                name: "FI_SURFACES",
                reason: "no surface".into(),
            });
        }
        match (out.contains(&Surface::Game), &pool) {
            (true, None) => {
                return Err(ConfigError::Invalid {
                    name: "FI_POOL",
                    reason: "the game surface serves exactly one pool".into(),
                });
            }
            (false, Some(_)) => {
                return Err(ConfigError::Invalid {
                    name: "FI_POOL",
                    reason: "only a game-surface deployment has a pool".into(),
                });
            }
            (true, Some(p)) if fi_domain::PoolName::new(p).is_err() => {
                return Err(ConfigError::Invalid {
                    name: "FI_POOL",
                    reason: "not a pool name".into(),
                });
            }
            _ => {}
        }
        Ok(Self {
            surfaces: out,
            platform: PlatformConfig::defaults(pool.clone()),
            pool,
        })
    }

    /// Whether the deployment serves `surface`.
    pub fn serves(&self, surface: Surface) -> bool {
        self.surfaces.contains(&surface)
    }

    /// Whether it needs the identity configuration (launcher or portal surface).
    pub fn serves_identity(&self) -> bool {
        self.serves(Surface::Launcher) || self.serves(Surface::Portal)
    }

    /// `launcher,portal` style label for `/meta`.
    pub fn surfaces_label(&self) -> String {
        self.surfaces
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Health (readiness = Postgres answers and the schema is this build's), meta, and the surfaces
/// the deployment serves (`FI_SURFACES`): `/launcher/v1` (identity, play sessions, saves) and
/// `/portal/v1`, and the game (`/game/v1`) and internal (`/internal/v1`) surfaces of the local
/// environment v0. The platform settings reach the handlers as a request extension
/// ([`Platform`]).
pub fn router(info: ServiceInfo, state: AppState, config: &Config) -> Router {
    let control_pool = state.pool.clone();
    let pool = state.pool.clone();
    let mut platform = config.platform.clone();
    platform.game_pool = config.pool.clone();
    let platform: Platform = Arc::new(platform);
    let mut api = Router::new();
    if config.serves(Surface::Launcher) {
        api = api.merge(launcher::router(&state));
        api = api.merge(crate::content::router());
    }
    if config.serves(Surface::Portal) {
        api = api.merge(portal::router(state.clone()));
    }
    if config.serves(Surface::Game) {
        api = api.merge(game::router());
    }
    if config.serves(Surface::Internal) {
        api = api.merge(internal::router());
    }
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
    let routes = http::health_router(ready)
        .merge(http::meta_router(info))
        .merge(api.with_state(state))
        .layer(Extension::<Platform>(platform));
    crate::online_control::protect(routes, control_pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use sqlx::postgres::PgPoolOptions;
    use std::time::Duration;
    use tower::ServiceExt;

    #[test]
    fn surfaces_and_pools_are_validated() {
        let account = Config::parse("launcher, portal,internal", None).unwrap();
        assert_eq!(account.surfaces_label(), "launcher,portal,internal");
        let game = Config::parse("game", Some("vip".into())).unwrap();
        assert_eq!(game.pool.as_deref(), Some("vip"));
        assert!(
            Config::parse("game", None).is_err(),
            "a game deployment names its pool"
        );
        assert!(
            Config::parse("launcher", Some("vip".into())).is_err(),
            "only game deployments have a pool"
        );
        assert!(Config::parse("launcher,admin", None).is_err());
        assert!(Config::parse(" , ", None).is_err());
        assert!(Config::parse("game", Some("VIP!".into())).is_err());
    }

    #[tokio::test]
    async fn readiness_follows_the_database_and_meta_describes_the_deployment() {
        // Nothing listens on port 9: the lazy pool fails to connect, so readiness is 503.
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(300))
            .connect_lazy("postgres://fi_app:x@127.0.0.1:9/forza_infinity")
            .unwrap();
        let info = ServiceInfo::from_env("fi-api", "0.0.0")
            .with_label("surfaces", "game")
            .with_label("pool", "vip");
        let config = Config::parse("game", Some("vip".into())).unwrap();
        let state = AppState {
            pool,
            identity: None,
        };
        assert_eq!(config.platform.game_pool.as_deref(), Some("vip"));
        let app = router(info, state, &config);
        let get = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        assert_eq!(
            app.clone()
                .oneshot(get("/health/live"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            app.clone()
                .oneshot(get("/health/ready"))
                .await
                .unwrap()
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let meta = app.oneshot(get("/meta")).await.unwrap();
        assert_eq!(meta.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(meta.into_body(), 64 * 1024).await.unwrap()).unwrap();
        assert_eq!(body["service"], "fi-api");
        assert_eq!(body["labels"]["pool"], "vip");
    }
}
