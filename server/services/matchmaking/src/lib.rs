//! fi-matchmaking: the platform v1 matchmaking control plane (ADR 0017 §1; PLATFORM_V1.md §3,
//! §4.5; MATCHMAKING_API.md). One router over fi-api's state: `/mm/v1` ([`routes`]),
//! `/health/live`, `/health/ready`, `/meta`, and, as the router's fallback, the free game plane's
//! proxied requests `/{host}/{path}` (GAME_PLATFORM.md §3a "Free tickets"; PV1-Q11, OD-34):
//! `fi_surfaces::game::proxy` with [`Features::Free`], always behind a valid ticket of a live, admitted
//! session: free roam, the free allowlist of the Forza RPC (logon, permissions, user data, the
//! empty inbox, the profile sync acknowledged and not stored), the Xbox Live JSON hosts and the
//! stubs; anything universe-shaped `GAME.NOT_SERVED`. The logic is the shared proxy module's.
//!
//! Every route but the public capacity and the relay detector's two routes ([`relay`]: signed by a
//! relay pinned in `FI_RELAYS`, MATCHMAKING_API.md §5d) takes the game access ticket, verify-only, of pool `vip`
//! or `matchmaking` and a game of `FI_GAMES` (`fi_surfaces::game::ticket`); tiers come from the
//! ticket. Decisions are one Postgres transaction each (`fi_db::matchmaking`); the service signs
//! the relay grants (`fi_auth::relay_grant`) the relay VM verifies offline. Behind the origin lock
//! (`FI_EDGE_SECRET`, ADR 0903 §1), so a host's address is `CF-Connecting-IP`, not Cloudflare's.
//! Per-instance derived state only: the liveness cache, the seen batch and the capacity cache
//! (10 s).

pub mod config;
pub mod official;
pub mod relay;
pub mod routes;
pub mod sync;

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::{Extension, Router};
use config::Settings;
use fi_auth::keys::SigningKeys;
use fi_protocol::game::FI_INSTANCE;
use fi_protocol::matchmaking::Capacity;
use fi_runtime::{ServiceInfo, http};
use fi_surfaces::game::cache::{LivenessCache, SeenBatch, TtlCache};
use fi_surfaces::game::proxy::{Features, Proxy, ProxyConfig};
use fi_surfaces::game::ticket::{TicketConfig, Tickets};
use fi_surfaces::online_control::OnlineGuard;
use fi_surfaces::state::{AppState, Platform};
use std::sync::Arc;
use std::time::Duration;

/// The pools a matchmaking ticket may carry.
pub const POOLS: [&str; 2] = ["vip", "matchmaking"];

/// Matchmaking's per-instance state (an `axum::Extension`).
#[derive(Debug)]
pub struct MmState {
    /// The relay-grant signing ring.
    pub grant_keys: SigningKeys,
    /// Games served.
    pub games: Vec<String>,
    /// The public capacity answers, 10 s.
    pub capacity: TtlCache<String, Capacity>,
    /// This instance (`X-Fi-Instance`).
    pub instance_id: String,
    /// `FI_MM_SEAT_HOLD_SECS`.
    pub seat_hold_secs: i64,
    /// `FI_MM_WORLD_IDLE_SECS`: a world with nobody but its host this long closes on its next
    /// heartbeat or `PUT` (`None`: off; OD-107).
    pub world_idle_secs: Option<i64>,
    /// `FI_MM_OFFICIAL_WAIT_MS`: how long a quick join or private create waits for an official
    /// world before 503 (CR-S100).
    pub official_wait_ms: u64,
    /// `FI_EDGE_LOCATION_HEADERS`: the edge sets the visitor location headers.
    pub location_headers: bool,
    /// `FI_RELAYS`: the relays this server trusts (CR-S57), in preference order.
    pub relays: Vec<config::Relay>,
    /// `FI_WORLDS`: the Worlds hosts this server trusts (OD-188/189, §2.9).
    pub worlds: Vec<config::WorldsHost>,
    /// Their identities, for `fi_db::matchmaking`'s relay choice.
    pub trusted: Vec<fi_db::matchmaking::TrustedRelay>,
    /// `FI_RELAY_SEATS_PER_WORLD`.
    pub relay_seats: u32,
    /// `FI_RELAY_DIRECTORY_URL`: the beholder whose relay list says which relays are down
    /// (OD-160; `None`: no check, none is down).
    pub relay_directory: Option<String>,
    /// The per-instance session key certified by the relay-grant key, signing the relay detector's
    /// answers (MATCHMAKING_API.md §5d).
    pub relay_sessions: relay::Sessions,
}

impl MmState {
    /// The trusted relay `key` (`None`: not, or no longer, trusted).
    pub fn relay(&self, key: &str) -> Option<&config::Relay> {
        self.relays.iter().find(|r| r.key == key)
    }
}

/// Shared handle.
pub type Mm = Arc<MmState>;

/// Everything the router installs.
#[derive(Clone)]
pub struct Parts {
    /// Ticket checks.
    pub tickets: Tickets,
    /// Matchmaking state.
    pub mm: Mm,
    /// The origin lock.
    pub edge: Option<Arc<fi_runtime::http::EdgeSecret>>,
    /// The proxied game requests: every feature since OD-129 (everything free for everyone; it
    /// was the free game plane, [`Features::Free`]), the hook minimum of `FI_MIN_HOOK`.
    pub proxy: Proxy,
    /// The game plane's bucket (ghost recordings) and guardrail switch, when this deployable runs
    /// inside a platform that has them (`fi-platform` sets the saves' ones); `None`: ghost uploads
    /// and downloads answer 503 here.
    pub game_platform: Option<Platform>,
}

impl Parts {
    /// From the settings.
    pub fn new(s: &Settings) -> Self {
        Self {
            tickets: Arc::new(TicketConfig {
                min_launcher: s.min_launcher.clone(),
                content_required: s.content_required,
                keys: s.verify_keys.clone(),
                // Matchmaking serves no launcher route: tickets only.
                access_keys: s.verify_keys.clone(),
                pools: s.pools.clone(),
                any_pool: false,
                games: s.games.clone(),
                liveness: Arc::new(LivenessCache::new(s.liveness_ms)),
                seen: Arc::new(SeenBatch::new(Duration::from_secs(60))),
                accounts: Default::default(),
            }),
            mm: Arc::new(MmState {
                grant_keys: s.grant_keys.clone(),
                games: s.games.clone(),
                capacity: TtlCache::new(Duration::from_secs(10), 64),
                instance_id: s.instance_id.clone(),
                seat_hold_secs: s.seat_hold_secs,
                world_idle_secs: s.world_idle_secs,
                official_wait_ms: s.official_wait_ms,
                location_headers: s.location_headers,
                relays: s.relays.clone(),
                worlds: s.worlds.clone(),
                relay_seats: s.relay_seats,
                relay_directory: s.relay_directory.clone(),
                relay_sessions: relay::Sessions::new(&s.grant_keys),
                trusted: s
                    .relays
                    .iter()
                    .map(|r| fi_db::matchmaking::TrustedRelay {
                        key: r.key.clone(),
                        name: r.name.clone(),
                    })
                    .collect(),
            }),
            edge: s.edge.clone(),
            proxy: Arc::new(
                ProxyConfig::new(Features::Supporter, s.min_hook.as_deref()).unwrap_or(
                    ProxyConfig {
                        features: Features::Supporter,
                        min_hook: None,
                    },
                ),
            ),
            game_platform: None,
        }
    }
}

async fn stamp(request: Request, next: Next) -> Response {
    let instance = request
        .extensions()
        .get::<Mm>()
        .map(|m| m.instance_id.clone());
    let mut response = next.run(request).await;
    if let Some(Ok(v)) = instance.as_deref().map(HeaderValue::from_str) {
        response.headers_mut().insert(FI_INSTANCE, v);
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
    let routes = with_extensions(
        http::health_router(ready)
            .merge(http::meta_router(info))
            .merge(
                routes::router()
                    .merge(relay::router())
                    .merge(official::router())
                    .merge(Router::new().fallback(fi_surfaces::game::proxy::serve))
                    .layer(middleware::from_fn(stamp))
                    .with_state(state),
            ),
        parts,
    );
    fi_surfaces::extract::origin_locked(
        fi_surfaces::online_control::protect(routes, control_pool),
        parts.edge.clone(),
    )
}

fn with_extensions(routes: Router, parts: &Parts) -> Router {
    routes
        .layer(Extension(parts.mm.clone()))
        .layer(Extension(parts.proxy.clone()))
        .layer(Extension(parts.tickets.clone()))
}

/// `/mm/v1` with this deployable's extensions and stamp, behind `online`: part of `fi-game`
/// (ADR 0023 §1). The free game plane is [`game_fallback`].
pub fn surface(state: AppState, parts: &Parts, online: &OnlineGuard) -> Router {
    online.guard(with_extensions(
        routes::router()
            .merge(relay::router())
            .merge(official::router())
            .layer(middleware::from_fn(stamp))
            .with_state(state),
        parts,
    ))
}

/// The free game plane's proxied requests (`/{host}/{path}`, [`Features::Free`]) as a router
/// whose fallback serves them, stamped and behind `online`, for a process that picks the game
/// plane by the ticket's pool (ADR 0023 §3).
pub fn game_fallback(state: AppState, parts: &Parts, online: &OnlineGuard) -> Router {
    let mut routes = Router::new()
        .fallback(fi_surfaces::game::proxy::serve)
        .layer(middleware::from_fn(stamp));
    if let Some(platform) = &parts.game_platform {
        routes = routes.layer(Extension(platform.clone()));
    }
    online.guard(with_extensions(routes.with_state(state), parts))
}

/// The background work of an instance: the liveness flush every 5 s and the matchmaking sweep
/// every 15 s (worlds past their TTL closed, seats past their time expired, invites and requests
/// past their time expired; `FOR UPDATE SKIP LOCKED`, so instances never block each other and the
/// hourly `gate-sweep` stays the backstop). Runs until the process ends.
pub async fn background(db: fi_db::PgPool, parts: Parts, instance: String) {
    // OD-160: with a beholder and trusted relays, its relay list every 30 s, in a task of its own
    // (a slow beholder never delays the flush or the sweep).
    if let (Some(url), false) = (&parts.mm.relay_directory, parts.mm.trusted.is_empty()) {
        match RelayDirectory::new(url) {
            Some(reader) => {
                tokio::spawn(relay_directory_loop(db.clone(), reader, parts.mm.clone()));
            }
            None => tracing::warn!("relay directory client cannot start; no relay counts as down"),
        }
    }
    let mut flush = tokio::time::interval(Duration::from_secs(5));
    let mut sweep = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = flush.tick() => {
                parts.tickets.seen.flush(&db, &instance).await;
            }
            _ = sweep.tick() => {
                if let Err(e) = sweep_once(&db).await {
                    tracing::warn!(error = %e, "matchmaking sweep failed; retried next tick");
                }
            }
        }
    }
}

/// The relay directory check every [`RELAY_DIRECTORY_EVERY`] (OD-160). Logged when the outcome
/// changes: counts and kinds, never the URL or a key.
async fn relay_directory_loop(db: fi_db::PgPool, reader: RelayDirectory, mm: Mm) {
    let mut every = tokio::time::interval(RELAY_DIRECTORY_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last = None;
    loop {
        every.tick().await;
        let outcome = reader.check(&db, &mm.trusted).await;
        if last != Some(outcome) {
            match outcome {
                Ok(listed) => {
                    tracing::info!(
                        listed,
                        trusted = mm.trusted.len(),
                        "relay directory checked"
                    );
                }
                Err(kind) => tracing::warn!(
                    error = kind.as_str(),
                    "relay directory check failed; no relay counts as down after 90 s"
                ),
            }
            last = Some(outcome);
        }
    }
}

/// How often an instance reads the beholder's relay list (OD-160; the relays' announce cadence).
pub const RELAY_DIRECTORY_EVERY: Duration = Duration::from_secs(30);
/// The largest relay list read (the beholder answers at most 500 items).
pub const RELAY_DIRECTORY_MAX_BYTES: usize = 1 << 20;

/// Why a relay directory check failed (logged as a kind, never with the URL).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryError {
    /// Connect, timeout or another transport error.
    Network,
    /// An answer other than 200.
    Status,
    /// Over [`RELAY_DIRECTORY_MAX_BYTES`], not a listing, or a full page
    /// ([`fi_protocol::beholder::MAX_LISTING_ITEMS`] items: a flood of throwaway announces may
    /// have pushed a trusted relay out of it, so it proves nothing).
    Body,
    /// The check could not be recorded.
    Database,
}

impl DirectoryError {
    /// The log kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Status => "status",
            Self::Body => "body",
            Self::Database => "database",
        }
    }
}

/// The beholder's relay list reader (OD-160, MATCHMAKING_API.md §5b "Relay by mode").
#[derive(Clone, Debug)]
pub struct RelayDirectory {
    url: String,
    client: reqwest::Client,
}

impl RelayDirectory {
    /// A reader of `{base}/beholder/v1/relays` (`None`: the HTTP client cannot start).
    pub fn new(base: &str) -> Option<Self> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        Some(Self {
            url: format!("{base}/beholder/v1/relays"),
            client,
        })
    }

    /// One check: reads the list (no database connection held meanwhile), then records for every
    /// trusted relay whether it was listed. Answers how many trusted relays were listed. A failed
    /// read records nothing, so the last check ages out after 90 s.
    pub async fn check(
        &self,
        db: &fi_db::PgPool,
        trusted: &[fi_db::matchmaking::TrustedRelay],
    ) -> Result<usize, DirectoryError> {
        let listed = self.read().await?;
        let listed: Vec<String> = trusted
            .iter()
            .filter(|t| listed.contains(&t.key))
            .map(|t| t.key.clone())
            .collect();
        fi_db::matchmaking::record_relay_directory(db, trusted, &listed)
            .await
            .map_err(|_| DirectoryError::Database)?;
        Ok(listed.len())
    }

    async fn read(&self) -> Result<Vec<String>, DirectoryError> {
        let mut answer = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|_| DirectoryError::Network)?;
        if answer.status() != reqwest::StatusCode::OK {
            return Err(DirectoryError::Status);
        }
        let mut body = Vec::new();
        while let Some(chunk) = answer.chunk().await.map_err(|_| DirectoryError::Network)? {
            if body.len() + chunk.len() > RELAY_DIRECTORY_MAX_BYTES {
                return Err(DirectoryError::Body);
            }
            body.extend_from_slice(&chunk);
        }
        let page: fi_protocol::beholder::ListingPage =
            serde_json::from_slice(&body).map_err(|_| DirectoryError::Body)?;
        if page.items.len() >= fi_protocol::beholder::MAX_LISTING_ITEMS {
            return Err(DirectoryError::Body);
        }
        Ok(page.items.into_iter().map(|l| l.public_key).collect())
    }
}

/// One matchmaking sweep in its own transaction.
pub async fn sweep_once(
    db: &fi_db::PgPool,
) -> Result<fi_db::matchmaking::Swept, fi_db::identity::IdentityError> {
    let mut tx = db.begin().await?;
    let swept = fi_db::matchmaking::sweep_in(&mut tx).await?;
    tx.commit().await?;
    if swept != fi_db::matchmaking::Swept::default() {
        tracing::info!(
            worlds_closed = swept.worlds_closed,
            grants_expired = swept.grants_expired,
            offers_expired = swept.offers_expired,
            "matchmaking sweep"
        );
    }
    Ok(swept)
}
