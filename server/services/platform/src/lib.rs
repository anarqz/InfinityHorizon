//! fi-platform: the three public APIs of ADR 0023 (OD-122) from one binary. `FI_SURFACES` (a
//! comma list of `infinity`, `game`, `progression`, or `all`) selects what a process mounts:
//!
//! - `infinity` (`fi-infinity`): fi-core's surface (identity, `/me`, `/core/v1`, `/portal/v1`,
//!   `/internal/v1`, `/mcp`), fi-gate's (`/gate/v1`, `/launcher/v1/sessions*`) and fi-crash's
//!   (`/crash/v1`), over one identity configuration that signs access tokens and launch tickets;
//! - `game` (`fi-game`): `/game/v1` and `/saves/v1` (the Supporters' universe), `/mm/v1`, and the
//!   game's proxied requests (`/{host}/{path}`) as the router's fallback, served with the
//!   Supporters' features when the ticket's pool is `vip` and the free ones otherwise
//!   ([`game_plane`]);
//! - `progression` (`fi-progression`): the save line (`/launcher/v1/profiles/{xuid}/save*`,
//!   `/launcher/v1/games/{game}/seeds/*`).
//!
//! Every surface keeps its own layers and extensions (inventory OD122_M0_INVENTORY.md §2); the
//! process adds one shell: `/health/live`, `/health/ready`, `/meta` (label `surfaces`), one
//! `/core/v1/online-status` and one online policy, behind the origin lock. Routers are joined
//! with `merge`, so each route keeps its exact path (`MatchedPath` keys gate's limits and the
//! matchmaking ticket check).
//!
//! A second, private listener (`FI_INTERNAL_BIND`, default `0.0.0.0:8081`, always on: OD-144)
//! serves only the play dashboard's reads (`/internal/dashboard/v1`, DASHBOARD_API.md, behind
//! `X-Fi-Dashboard-Key` = `FI_DASHBOARD_KEY`; refused when unset) ([`Instance::internal_router`]).
//! They are never on the public port, which the tunnel reaches.

pub mod announce;
pub mod keys;
pub mod ops;

use axum::Router;
use axum::extract::Request;
use axum::response::Response;
use fi_db::PgPool;
use fi_runtime::ServiceInfo;
use fi_runtime::config::ConfigError;
use fi_runtime::http::{self, EdgeSecret};
use fi_surfaces::config::{Env, IdentityConfig, IdentityNeeds, PlatformConfig};
use fi_surfaces::dashboard::DashboardKey;
use fi_surfaces::game::ticket::Tickets;
use fi_surfaces::online_control::OnlineGuard;
use fi_surfaces::state::AppState;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

/// `FI_INTERNAL_BIND` when unset.
pub const DEFAULT_INTERNAL_BIND: &str = "0.0.0.0:8081";

/// The private listener's settings.
#[derive(Clone, Debug)]
pub struct InternalListener {
    /// `FI_INTERNAL_BIND`.
    pub bind: SocketAddr,
    /// `FI_DASHBOARD_KEY` (`None`: every dashboard request is 401).
    pub dashboard_key: Option<DashboardKey>,
}

impl InternalListener {
    /// Reads `FI_INTERNAL_BIND` (an `ip:port`; default [`DEFAULT_INTERNAL_BIND`]) and
    /// `FI_DASHBOARD_KEY`.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        let text = env
            .get("FI_INTERNAL_BIND")
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| DEFAULT_INTERNAL_BIND.to_owned());
        let bind = text.parse().map_err(|_| ConfigError::Invalid {
            name: "FI_INTERNAL_BIND",
            reason: "an ip:port, for example 0.0.0.0:8081".into(),
        })?;
        // The key: blank or unset is no key (every dashboard request refused, fail closed); a
        // key shorter than 32 characters refuses to start (DASHBOARD_API.md §5.1 "As built").
        let key = env.get("FI_DASHBOARD_KEY").unwrap_or_default();
        let dashboard_key = match fi_surfaces::dashboard::checked_key(&key) {
            Ok(Some(text)) => DashboardKey::new(text),
            Ok(None) => None,
            Err(reason) => {
                return Err(ConfigError::Invalid {
                    name: "FI_DASHBOARD_KEY",
                    reason: reason.into(),
                });
            }
        };
        Ok(Self {
            bind,
            dashboard_key,
        })
    }
}

/// The surfaces a process mounts (`FI_SURFACES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surfaces {
    /// `fi-infinity`.
    pub infinity: bool,
    /// `fi-game`.
    pub game: bool,
    /// `fi-progression`.
    pub progression: bool,
}

impl Surfaces {
    /// Every surface: a standalone instance.
    pub const ALL: Self = Self {
        infinity: true,
        game: true,
        progression: true,
    };

    /// A comma list of `infinity`, `game`, `progression` or `all`; at least one.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut s = Self {
            infinity: false,
            game: false,
            progression: false,
        };
        for word in text.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            match word {
                "all" => s = Self::ALL,
                "infinity" => s.infinity = true,
                "game" => s.game = true,
                "progression" => s.progression = true,
                _ => {
                    return Err(ConfigError::Invalid {
                        name: "FI_SURFACES",
                        reason: "a comma list of infinity, game, progression, or all".into(),
                    });
                }
            }
        }
        if !(s.infinity || s.game || s.progression) {
            return Err(ConfigError::Missing("FI_SURFACES"));
        }
        Ok(s)
    }

    /// The `surfaces` label of `/meta`.
    pub fn label(&self) -> String {
        [
            (self.infinity, "infinity"),
            (self.game, "game"),
            (self.progression, "progression"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(",")
    }

    /// The service name `/meta` reports: the Cloud Run service of a single surface, else
    /// `fi-platform`.
    pub fn service(&self) -> &'static str {
        match (self.infinity, self.game, self.progression) {
            (true, false, false) => "fi-infinity",
            (false, true, false) => "fi-game",
            (false, false, true) => "fi-progression",
            _ => "fi-platform",
        }
    }
}

/// `fi-infinity`'s settings.
pub struct Infinity {
    /// Sign-in, access tokens (signing), launch tickets, site keys, `/launcher/v1/info`.
    pub identity: Arc<IdentityConfig>,
    /// fi-core's platform settings.
    pub platform: PlatformConfig,
    /// fi-core's own state.
    pub core: Arc<fi_core::Core>,
    /// fi-gate's settings over the same identity.
    pub gate: fi_gate::config::GateConfig,
}

/// Everything a process reads at startup.
pub struct Config {
    /// `FI_SURFACES`.
    pub surfaces: Surfaces,
    /// With `infinity`.
    pub infinity: Option<Infinity>,
    /// fi-saves' settings, with `game` or `progression`.
    pub saves: Option<fi_saves_service::config::Settings>,
    /// fi-matchmaking's settings, with `game`.
    pub mm: Option<fi_matchmaking::config::Settings>,
    /// The origin lock (`FI_EDGE_SECRET`).
    pub edge: Option<Arc<EdgeSecret>>,
    /// The private listener (`FI_INTERNAL_BIND`, `FI_DASHBOARD_KEY`).
    pub internal: InternalListener,
}

impl Config {
    /// Reads `FI_SURFACES` and the settings of each mounted surface.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        let surfaces = Surfaces::parse(&env.get("FI_SURFACES").unwrap_or_default())?;
        let infinity = if surfaces.infinity {
            let identity = Arc::new(IdentityConfig::from_env_for(
                env,
                IdentityNeeds::combined(false),
            )?);
            // fi-core's local administration is not the gate's: the gate keeps refusing it.
            let gate_env = View {
                inner: env,
                hidden: &["FI_LOCAL_ADMIN"],
                set: Vec::new(),
            };
            Some(Infinity {
                platform: PlatformConfig::from_env(env, None, false, false)?,
                core: Arc::new(fi_core::Core::new(fi_core::config::CoreConfig::from_env(
                    env,
                )?)),
                gate: fi_gate::config::GateConfig::from_env_with(&gate_env, identity.clone())?,
                identity,
            })
        } else {
            None
        };
        // With fi-infinity in the same process the game surfaces verify with the public halves of
        // the rings it signs with (ADR 0023 §4), and never see the private ones.
        let verify = match &infinity {
            Some(inf) => verify_view(env, inf)?,
            None => View {
                inner: env,
                hidden: &[],
                set: Vec::new(),
            },
        };
        let saves = if surfaces.game || surfaces.progression {
            Some(fi_saves_service::config::Settings::from_env(&verify)?)
        } else {
            None
        };
        let mm = if surfaces.game {
            Some(fi_matchmaking::config::Settings::from_env(&verify)?)
        } else {
            None
        };
        Ok(Self {
            surfaces,
            infinity,
            saves,
            mm,
            edge: fi_surfaces::config::edge_secret(env)?,
            internal: InternalListener::from_env(env)?,
        })
    }
}

/// `inner` with some variables hidden and some set.
struct View<'a> {
    inner: &'a dyn Env,
    hidden: &'static [&'static str],
    set: Vec<(&'static str, String)>,
}

impl Env for View<'_> {
    fn get(&self, name: &'static str) -> Option<String> {
        if let Some((_, value)) = self.set.iter().find(|(n, _)| *n == name) {
            return Some(value.clone());
        }
        if self.hidden.contains(&name) {
            return None;
        }
        self.inner.get(name)
    }
}

/// What the game surfaces never read beside fi-infinity: the signing rings (their public halves
/// are set instead) and the `local` IdP's ring (ADR 0023 §9).
const GAME_HIDDEN: &[&str] = &[
    "FI_TOKEN_SIGNING_KEYS",
    "FI_PLATFORM_SIGNING_KEYS",
    "FI_LOCAL_IDP_SIGNING_KEYS",
];

/// The environment the game surfaces read beside fi-infinity: the signing rings hidden, their
/// public halves as the verify variables.
fn verify_view<'a>(env: &'a dyn Env, inf: &Infinity) -> Result<View<'a>, ConfigError> {
    let tickets = inf
        .gate
        .platform
        .session_keys
        .as_ref()
        .ok_or(ConfigError::Missing("FI_TOKEN_SIGNING_KEYS"))?;
    let access = inf
        .identity
        .access_keys
        .signing()
        .ok_or(ConfigError::Missing("FI_PLATFORM_SIGNING_KEYS"))?;
    let set = vec![
        (
            "FI_TOKEN_VERIFY_KEYS",
            fi_auth::session::VerifyKeys::of(tickets).to_text(),
        ),
        (
            "FI_PLATFORM_VERIFY_KEYS",
            fi_auth::session::VerifyKeys::of(access).to_text(),
        ),
    ];
    // A verify variable set beside its signing ring must say the same: one that names other keys
    // (a rotation's old kid) would be dropped silently otherwise.
    for (name, derived) in &set {
        if env.get(name).is_some_and(|text| text.trim() != derived) {
            return Err(ConfigError::Invalid {
                name,
                reason: "differs from the public half of the signing ring this process holds: \
                         leave it unset (it is derived) or add the kid to the signing ring"
                    .into(),
            });
        }
    }
    Ok(View {
        inner: env,
        hidden: GAME_HIDDEN,
        set,
    })
}

/// A configured process: the per-instance state its surfaces share.
pub struct Instance {
    config: Config,
    saves: Option<fi_saves_service::Parts>,
    mm: Option<fi_matchmaking::Parts>,
}

/// The game plane's two configurations, picked per request by the ticket's pool.
struct Dispatch {
    tickets: Tickets,
    supporter: Router,
    free: Router,
}

/// The game's proxied requests (ADR 0023 §3): a ticket of pool `vip` reaches fi-saves' game plane
/// (the Supporters' features, narrowed per ticket as before), any other request fi-matchmaking's
/// free one, whose checks answer a missing, invalid or foreign ticket exactly as before. The pool
/// is read from the ticket's verified claims; the chosen plane verifies it again with its own
/// pools, liveness and games.
async fn game_plane(dispatch: Arc<Dispatch>, request: Request) -> Response {
    let vip = request
        .headers()
        .get(fi_protocol::headers::FI_SESSION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .and_then(|token| fi_surfaces::game::ticket::verify(&dispatch.tickets.keys, token).ok())
        .is_some_and(|t| t.claims.pool == fi_saves_service::POOL);
    let plane = if vip {
        dispatch.supporter.clone()
    } else {
        dispatch.free.clone()
    };
    match plane.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

impl Instance {
    /// The per-instance state of `config`'s surfaces.
    pub fn new(config: Config) -> Self {
        let saves = config.saves.as_ref().map(fi_saves_service::Parts::new);
        let mut mm = config.mm.as_ref().map(fi_matchmaking::Parts::new);
        // OD-129: the matchmaking pool's game plane serves every feature, so it gets the saves'
        // bucket and guardrail switch (its own pool stamp stays).
        if let (Some(saves), Some(mm)) = (&saves, &mut mm) {
            let mut platform = (*saves.platform).clone();
            platform.game_pool = Some("matchmaking".to_owned());
            mm.game_platform = Some(Arc::new(platform));
        }
        Self { saves, mm, config }
    }

    /// The mounted surfaces.
    pub fn surfaces(&self) -> Surfaces {
        self.config.surfaces
    }

    /// `/meta`: the service name, the labels each mounted deployable reported, `surfaces` and
    /// `edge_lock`.
    pub fn info(&self) -> ServiceInfo {
        let mut info =
            ServiceInfo::from_env(self.config.surfaces.service(), env!("CARGO_PKG_VERSION"));
        if let Some(inf) = &self.config.infinity {
            for (k, v) in fi_gate::service_info(&inf.gate).labels {
                info = info.with_label(k, v);
            }
            let emulator = inf
                .identity
                .verifier
                .as_ref()
                .is_some_and(|v| v.is_emulator());
            let idp = if inf.identity.is_local() {
                "local"
            } else if emulator {
                "emulator"
            } else {
                "rs256"
            };
            info = info
                .with_label("idp", idp)
                .with_label("turnstile", inf.core.turnstile.mode())
                .with_label("mcp", inf.core.config.mcp.enabled.to_string());
        }
        if let Some(saves) = &self.config.saves {
            info = info
                .with_label("games", saves.games.join(","))
                .with_label("liveness_cache_ms", saves.liveness_ms.to_string())
                .with_label("storage", saves.storage_label());
        }
        info.with_label("surfaces", self.config.surfaces.label())
            .with_label("edge_lock", self.config.edge.is_some().to_string())
    }

    /// The process's router over `pool`.
    pub fn router(&self, info: ServiceInfo, pool: PgPool) -> Router {
        let online = OnlineGuard::new(pool.clone());
        let mut api = Router::new();
        if let Some(inf) = &self.config.infinity {
            let state = AppState {
                pool: pool.clone(),
                identity: Some(inf.identity.clone()),
            };
            api = api
                .merge(fi_core::surface(
                    state.clone(),
                    inf.platform.clone(),
                    inf.core.clone(),
                    &online,
                ))
                .merge(fi_gate::surface(state.clone(), &inf.gate, &online))
                .merge(fi_crash_service::surface(state, &online));
        }
        let state = AppState {
            pool: pool.clone(),
            identity: None,
        };
        if let Some(saves) = &self.saves {
            if self.config.surfaces.progression {
                api = api.merge(fi_saves_service::progression_surface(
                    state.clone(),
                    saves,
                    &online,
                ));
            }
            if self.config.surfaces.game {
                api = api.merge(fi_saves_service::game_surface(
                    state.clone(),
                    saves,
                    &online,
                ));
            }
        }
        api = match (&self.saves, &self.mm) {
            (Some(saves), Some(mm)) => {
                let dispatch = Arc::new(Dispatch {
                    tickets: mm.tickets.clone(),
                    supporter: fi_saves_service::game_fallback(state.clone(), saves, &online),
                    free: fi_matchmaking::game_fallback(state.clone(), mm, &online),
                });
                api.merge(fi_matchmaking::surface(state, mm, &online))
                    .fallback(move |request: Request| game_plane(dispatch.clone(), request))
            }
            // Without the game plane an unknown path is a 404, behind the online policy as
            // every deployable's was.
            _ => api.fallback_service(online.guard(Router::new())),
        };
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
        let shell = http::health_router(ready)
            .merge(http::meta_router(info))
            .merge(online.status_router());
        // fi-core's browser callers read the online status and the shell as before.
        let shell = match &self.config.infinity {
            Some(inf) => fi_core::browser(shell, inf.core.clone()),
            None => shell,
        };
        let routes = shell.merge(api);
        fi_surfaces::extract::origin_locked(routes, self.config.edge.clone())
    }

    /// The private listener's address (`FI_INTERNAL_BIND`).
    pub fn internal_bind(&self) -> SocketAddr {
        self.config.internal.bind
    }

    /// The private listener's router over `pool`: the play dashboard's reads only
    /// (`/internal/dashboard/v1`), whatever the surfaces. No online policy (the site shows the
    /// server closed too) and no origin lock (the edge never reaches it): the dashboard key guards
    /// it. Every other path is the same 401.
    pub fn internal_router(&self, pool: PgPool) -> Router {
        fi_surfaces::dashboard::router(pool, self.config.internal.dashboard_key.clone())
    }

    /// Starts the background work of the game surface (fi-saves' and fi-matchmaking's liveness
    /// flushes, the matchmaking sweep, the universe metrics); nothing for the other surfaces.
    pub fn spawn_background(&self, pool: &PgPool) {
        if !self.config.surfaces.game {
            return;
        }
        if let (Some(parts), Some(settings)) = (&self.saves, &self.config.saves) {
            tokio::spawn(fi_saves_service::background(
                pool.clone(),
                parts.clone(),
                settings.instance_id.clone(),
            ));
        }
        if let (Some(parts), Some(settings)) = (&self.mm, &self.config.mm) {
            tokio::spawn(fi_matchmaking::background(
                pool.clone(),
                parts.clone(),
                settings.instance_id.clone(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surfaces_parse() {
        assert_eq!(Surfaces::parse("all").unwrap(), Surfaces::ALL);
        let g = Surfaces::parse(" game ").unwrap();
        assert!(g.game && !g.infinity && !g.progression);
        assert_eq!(g.service(), "fi-game");
        let two = Surfaces::parse("infinity,progression").unwrap();
        assert_eq!(two.label(), "infinity,progression");
        assert_eq!(two.service(), "fi-platform");
        assert_eq!(Surfaces::ALL.label(), "infinity,game,progression");
        assert!(Surfaces::parse("").is_err());
        assert!(Surfaces::parse("core").is_err());
    }

    #[test]
    fn the_game_view_hides_every_signing_ring() {
        let env = [
            ("FI_TOKEN_SIGNING_KEYS", "at-1:x"),
            ("FI_PLATFORM_SIGNING_KEYS", "pk-1:x"),
            ("FI_LOCAL_IDP_SIGNING_KEYS", "idp-1:x"),
            ("FI_REALM", "realm"),
        ];
        let view = View {
            inner: &env,
            hidden: GAME_HIDDEN,
            set: vec![("FI_PLATFORM_VERIFY_KEYS", "pk-1:pub".into())],
        };
        for hidden in [
            "FI_TOKEN_SIGNING_KEYS",
            "FI_PLATFORM_SIGNING_KEYS",
            "FI_LOCAL_IDP_SIGNING_KEYS",
        ] {
            assert_eq!(view.get(hidden), None, "{hidden}");
        }
        assert_eq!(view.get("FI_REALM").as_deref(), Some("realm"));
        assert_eq!(
            view.get("FI_PLATFORM_VERIFY_KEYS").as_deref(),
            Some("pk-1:pub")
        );
    }
}
