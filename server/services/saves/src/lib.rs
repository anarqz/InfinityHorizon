//! fi-saves-service: the platform v1 saves deployable (ADR 0013 §1, ADR 0016; PLATFORM_V1.md §3,
//! §4.4; UNIVERSE_API.md). One router over fi-api's state:
//!
//! - `/launcher/v1/profiles/{xuid}/save*`: the save line of SAVES_API.md §4
//!   (`fi_surfaces::launcher::saves`), the launcher's platform session verified with public keys;
//! - `/game/v1`: the game plane's platform surface (`fi_surfaces::game`), pool `vip`;
//! - `/saves/v1`: the Supporters' universe ([`universe`]);
//! - `/{host}/{path}`: the game's own proxied requests (GAME_PLATFORM.md §3a), every feature built
//!   (`fi_surfaces::game::proxy`, [`Features::Supporter`]: free roam, Forza RPC, Xbox Live JSON,
//!   stubs), mounted as the router's fallback so it never shadows a route above;
//! - `/health/live`, `/health/ready`, `/meta`;
//!
//! behind the origin lock (`FI_EDGE_SECRET`, ADR 0903 §1; fi-api's `origin_locked`).
//!
//! Every game route takes the game access ticket, verify-only ([`fi_surfaces::game::ticket`]): pool
//! `vip` and a game of `FI_GAMES`, else `GAME.WRONG_POOL` before anything else. Answers of
//! `/game/v1` and `/saves/v1` carry `X-Fi-Pool: vip` and `X-Fi-Instance`. Per-instance derived
//! state only: the liveness cache, the seen batch and the read caches.

pub mod config;
pub mod universe;

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::{Extension, Router};
use config::Settings;
use fi_protocol::game::{FI_INSTANCE, FI_POOL};
use fi_runtime::{ServiceInfo, http};
use fi_storage::{Blobs, GcsBlobStore};
use fi_surfaces::config::PlatformConfig;
use fi_surfaces::game::cache::{LivenessCache, SeenBatch};
use fi_surfaces::game::proxy::{Features, Proxy, ProxyConfig};
use fi_surfaces::game::ticket::{TicketConfig, Tickets};
use fi_surfaces::online_control::OnlineGuard;
use fi_surfaces::state::{AppState, Platform};
use std::sync::Arc;
use std::time::Duration;

/// The pool this deployable serves.
pub const POOL: &str = "vip";

/// The universe's per-instance state (an `axum::Extension`).
#[derive(Debug)]
pub struct UniverseState {
    /// Read caches and counters.
    pub caches: universe::Caches,
}

/// Shared handle.
pub type Universe = Arc<UniverseState>;

/// Everything the router installs, built once from [`Settings`].
#[derive(Clone)]
pub struct Parts {
    /// Ticket checks.
    pub tickets: Tickets,
    /// Universe state.
    pub universe: Universe,
    /// fi-api's platform settings (the bucket, the pool and instance stamps).
    pub platform: Platform,
    /// The origin lock.
    pub edge: Option<Arc<fi_runtime::http::EdgeSecret>>,
    /// The proxied game requests: every feature, the hook minimum of `FI_MIN_HOOK`.
    pub proxy: Proxy,
}

impl Parts {
    /// From the settings (`min_hook` was checked by [`Settings::from_env`]; an unparsable one
    /// set by hand is ignored).
    pub fn new(s: &Settings) -> Self {
        let blobs: Blobs = match (&s.fs_blobs, &s.storage_endpoint) {
            (Some(fs), _) => fs.clone(),
            (None, Some(endpoint)) => GcsBlobStore::new(endpoint.clone(), s.bucket.clone()).into(),
            (None, None) => GcsBlobStore::cloud(s.bucket.clone()).into(),
        };
        let mut platform = PlatformConfig::defaults(Some(POOL.to_owned()));
        platform.instance_id = s.instance_id.clone();
        platform.blobs = Some(blobs);
        platform.guard_enforce = s.guard_enforce;
        let mut platform = platform.with_cloud_saves(s.cloud_saves);
        platform.content_required = s.content_required;
        platform.min_launcher = s.min_launcher.clone();
        let liveness = Arc::new(LivenessCache::new(s.liveness_ms));
        platform.liveness = liveness.clone();
        Self {
            tickets: Arc::new(TicketConfig {
                min_launcher: s.min_launcher.clone(),
                content_required: s.content_required,
                keys: s.verify_keys.clone(),
                access_keys: s.access_keys.clone(),
                pools: vec![POOL.to_owned()],
                any_pool: true,
                games: s.games.clone(),
                liveness,
                seen: Arc::new(SeenBatch::new(Duration::from_secs(60))),
                accounts: s.accounts.clone(),
            }),
            universe: Arc::new(UniverseState {
                caches: universe::Caches::new(s.ttls),
            }),
            platform: Arc::new(platform),
            edge: s.edge.clone(),
            proxy: Arc::new(
                ProxyConfig::new(Features::Supporter, s.min_hook.as_deref()).unwrap_or(
                    ProxyConfig {
                        features: Features::Supporter,
                        min_hook: None,
                    },
                ),
            ),
        }
    }
}

async fn stamp(request: Request, next: Next) -> Response {
    let instance = request
        .extensions()
        .get::<Platform>()
        .map(|p| p.instance_id.clone());
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(FI_POOL, HeaderValue::from_static(POOL));
    if let Some(Ok(v)) = instance.as_deref().map(HeaderValue::from_str) {
        h.insert(FI_INSTANCE, v);
    }
    response
}

/// The deployable's router.
pub fn router(info: ServiceInfo, state: AppState, parts: &Parts) -> Router {
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
    let api = Router::new()
        .merge(fi_surfaces::launcher::saves::router())
        .merge(fi_surfaces::game::router())
        .merge(universe::router().layer(middleware::from_fn(stamp)))
        .merge(
            Router::new()
                .fallback(fi_surfaces::game::proxy::serve)
                .layer(middleware::from_fn(stamp)),
        )
        .with_state(state);
    let routes = with_extensions(
        http::health_router(ready)
            .merge(http::meta_router(info))
            .merge(api),
        parts,
    );
    fi_surfaces::extract::origin_locked(
        fi_surfaces::online_control::protect(routes, control_pool),
        parts.edge.clone(),
    )
}

fn with_extensions(routes: Router, parts: &Parts) -> Router {
    routes
        .layer(Extension(parts.universe.clone()))
        .layer(Extension(parts.proxy.clone()))
        .layer(Extension(parts.tickets.clone()))
        .layer(Extension(parts.platform.clone()))
}

/// The save line (`/launcher/v1/profiles/{xuid}/save*`, `/launcher/v1/games/{game}/seeds/*`)
/// with this deployable's extensions, behind `online`: `fi-progression` (ADR 0023 §1). No
/// health, meta or origin lock.
pub fn progression_surface(state: AppState, parts: &Parts, online: &OnlineGuard) -> Router {
    online.guard(with_extensions(
        fi_surfaces::launcher::saves::router().with_state(state),
        parts,
    ))
}

/// The Supporters' game routes (`/game/v1`, `/saves/v1`) with this deployable's extensions and
/// stamps, behind `online`: part of `fi-game` (ADR 0023 §1). The proxied requests are
/// [`game_fallback`].
pub fn game_surface(state: AppState, parts: &Parts, online: &OnlineGuard) -> Router {
    online.guard(with_extensions(
        Router::new()
            .merge(fi_surfaces::game::router())
            .merge(universe::router().layer(middleware::from_fn(stamp)))
            .with_state(state),
        parts,
    ))
}

/// The Supporters' proxied game requests (`/{host}/{path}`, [`Features::Supporter`]) as a router
/// whose fallback serves them, stamped and behind `online`, for a process that picks the game
/// plane by the ticket's pool (ADR 0023 §3).
pub fn game_fallback(state: AppState, parts: &Parts, online: &OnlineGuard) -> Router {
    online.guard(with_extensions(
        Router::new()
            .fallback(fi_surfaces::game::proxy::serve)
            .layer(middleware::from_fn(stamp))
            .with_state(state),
        parts,
    ))
}

/// The background work of an instance: the liveness flush every 5 s, the metrics line every
/// minute. Runs until the process ends.
pub async fn background(db: fi_db::PgPool, parts: Parts, instance: String) {
    let mut flush = tokio::time::interval(universe::FLUSH_EVERY);
    let mut metrics = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = flush.tick() => {
                parts.tickets.seen.flush(&db, &instance).await;
            }
            _ = metrics.tick() => universe::log_metrics(&parts.universe.caches),
        }
    }
}
