//! Server details (CORE_API.md §2.2, §2.3, §2.4): `GET /core/v1/status`, the notice board
//! `GET /core/v1/notices` and the server summary `GET /core/v1/summary`, all public and
//! edge-cacheable.
//!
//! `queue` is the gate's summary read through Postgres (`fi_db::notices::queue_summary_in`);
//! `live` (CR-L40) the game's aggregates in one statement (`fi_db::notices::live_in`);
//! `stats` is the gold snapshot, `null` until the saves lane's `gold_snapshot` table and its
//! 3-day build exist (a stub here: core never queries BigQuery); `services` are readiness probes
//! of the other deployables, cached per instance for 60 s (derived state whose loss costs one
//! probe round, services/AGENTS.md).

use crate::Core;
use crate::config::ProbeTargets;
use axum::Json;
use axum::extract::{Extension, Query, State};
use axum::http::header::CACHE_CONTROL;
use fi_db::notices::{self, GameRow, NoticeRow};
use fi_protocol::ErrorCode;
use fi_protocol::core::{
    GameLive, GameStatus, ModeCounts, Notice, NoticeList, QueueCounts, ServerSummary,
    ServiceHealth, Status,
};
use fi_surfaces::errors::ApiError;
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one probe round is reused.
pub const PROBE_TTL: Duration = Duration::from_secs(60);
/// Timeout of one readiness probe.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// `Cache-Control` of the status answer.
pub const STATUS_CACHE: &str = "public, max-age=10, s-maxage=10";
/// `Cache-Control` of the server summary (its nightly part changes once a day).
pub const SUMMARY_CACHE: &str = "public, max-age=300, s-maxage=300";
/// `Cache-Control` of the notice board.
pub const NOTICES_CACHE: &str = "public, max-age=60, s-maxage=60";

/// The readiness probes and their per-instance cache.
pub struct Probes {
    targets: ProbeTargets,
    client: reqwest::Client,
    cache: tokio::sync::Mutex<Option<(Instant, ServiceHealth)>>,
}

impl Probes {
    /// Probes of `targets`.
    pub fn new(targets: ProbeTargets) -> Self {
        Self {
            targets,
            client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .build()
                .unwrap_or_default(),
            cache: tokio::sync::Mutex::new(None),
        }
    }

    async fn probe(&self, url: &Option<String>) -> String {
        let Some(url) = url else {
            return "unknown".into();
        };
        match self.client.get(url).send().await {
            Ok(r) if r.status().is_success() => "ok".into(),
            _ => "down".into(),
        }
    }

    /// The last round, or a new one when it is older than [`PROBE_TTL`] (concurrent callers wait
    /// for the one round in flight).
    pub async fn health(&self) -> ServiceHealth {
        let mut cache = self.cache.lock().await;
        if let Some((at, health)) = cache.as_ref()
            && at.elapsed() < PROBE_TTL
        {
            return health.clone();
        }
        let (gate, saves, matchmaking, relay) = tokio::join!(
            self.probe(&self.targets.gate),
            self.probe(&self.targets.saves),
            self.probe(&self.targets.matchmaking),
            self.probe(&self.targets.relay),
        );
        let health = ServiceHealth {
            gate,
            saves,
            matchmaking,
            relay,
        };
        *cache = Some((Instant::now(), health.clone()));
        health
    }
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn live(g: &GameRow, live: &[notices::LiveRow]) -> Option<GameLive> {
    live.iter().find(|l| l.game == g.code).map(|l| GameLive {
        online: count(l.online),
        capacity: l.capacity.and_then(|c| u32::try_from(c).ok()),
        hardcore: count(l.hardcore),
        noob: count(l.noob),
        lobbies: count(l.lobbies),
        profiles: ModeCounts {
            hardcore: count(l.profiles_hardcore),
            noob: count(l.profiles_noob),
        },
    })
}

fn game_status(g: &GameRow, queue: &[notices::QueueRow], lives: &[notices::LiveRow]) -> GameStatus {
    GameStatus {
        live: live(g, lives),
        id: g.code.clone(),
        accepting: g.accepting && g.maintenance_until.is_none(),
        maintenance_until: g.maintenance_until.map(views::at),
        queue: queue
            .iter()
            .filter(|q| q.game == g.code)
            .map(|q| {
                (
                    q.pool.clone(),
                    QueueCounts {
                        active: q.active.max(0) as u64,
                        queued: q.queued.max(0) as u64,
                        capacity: q.capacity.map(|c| c.max(0) as u32),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
    }
}

/// `GET /core/v1/status` → 200 [`Status`]; `maintenance` when a game is in maintenance, else
/// `degraded` when a probed service is down, else `ok`.
pub async fn status(
    State(state): State<AppState>,
    Extension(core): Extension<Arc<Core>>,
) -> Result<([(axum::http::HeaderName, &'static str); 1], Json<Status>), ApiError> {
    let (games, queue, lives) = {
        let mut conn = state.pool.acquire().await?;
        (
            notices::listed_games_in(&mut conn).await?,
            notices::queue_summary_in(&mut conn).await?,
            notices::live_in(&mut conn).await?,
        )
    };
    let services = core.probes.health().await;
    let games: Vec<GameStatus> = games
        .iter()
        .map(|g| game_status(g, &queue, &lives))
        .collect();
    let down = [
        &services.gate,
        &services.saves,
        &services.matchmaking,
        &services.relay,
    ]
    .iter()
    .any(|s| s.as_str() == "down");
    let overall = if games.iter().any(|g| g.maintenance_until.is_some()) {
        "maintenance"
    } else if down {
        "degraded"
    } else {
        "ok"
    };
    Ok((
        [(CACHE_CONTROL, STATUS_CACHE)],
        Json(Status {
            status: overall.into(),
            generated_at: views::at(fi_domain::UnixMs::now_system()),
            games,
            stats: None,
            services,
        }),
    ))
}

/// A board notice on the wire.
pub fn notice(n: &NoticeRow) -> Notice {
    Notice {
        id: n.id.to_string(),
        game: n.game.clone(),
        kind: n.kind.clone(),
        level: n.level.clone(),
        title: n.title.clone(),
        body: n.body.clone(),
        lang: n.lang.clone(),
        since: views::at(n.since),
        until: n.until.map(views::at),
    }
}

/// The maintenance notice derived from a game's switch (never a row).
pub fn maintenance_notice(g: &GameRow, lang: &str, now: fi_domain::UnixMs) -> Option<Notice> {
    let until = g.maintenance_until?;
    let (title, body) = match lang {
        "pt-BR" => (
            "Manutenção",
            g.maintenance_message
                .clone()
                .unwrap_or_else(|| format!("Servidores em manutenção até {}.", views::at(until))),
        ),
        _ => (
            "Maintenance",
            g.maintenance_message.clone().unwrap_or_else(|| {
                format!("Servers are under maintenance until {}.", views::at(until))
            }),
        ),
    };
    Some(Notice {
        id: format!("maintenance:{}", g.code),
        game: Some(g.code.clone()),
        kind: "maintenance".into(),
        level: "warning".into(),
        title: title.into(),
        body,
        lang: lang.into(),
        since: views::at(now),
        until: Some(views::at(until)),
    })
}

/// `?game=&lang=` of the board.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardQuery {
    /// Game (required).
    pub game: Option<String>,
    /// `en` (default) | `pt-BR`.
    pub lang: Option<String>,
}

/// A `lang` parameter: `en` | `pt-BR`.
pub fn lang(value: Option<&str>) -> Result<&'static str, ApiError> {
    match value.unwrap_or("en") {
        "en" => Ok("en"),
        "pt-BR" => Ok("pt-BR"),
        _ => Err(ApiError::invalid("lang", "en or pt-BR")),
    }
}

/// `GET /core/v1/notices?game=fh2&lang=en` → 200 [`NoticeList`]: the derived maintenance notice
/// first, then the game's and platform-wide notices, newest first. `PORTAL.NOT_FOUND` for a game
/// this deployment does not serve.
pub async fn board(
    State(state): State<AppState>,
    query: Result<Query<BoardQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<
    (
        [(axum::http::HeaderName, &'static str); 1],
        Json<NoticeList>,
    ),
    ApiError,
> {
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "game and lang only"))?;
    let game = q
        .game
        .filter(|g| crate::valid_game(g))
        .ok_or_else(|| ApiError::invalid("game", "a game id such as fh2"))?;
    let lang = lang(q.lang.as_deref())?;
    let (row, board) = {
        let mut conn = state.pool.acquire().await?;
        let row = notices::game_in(&mut conn, &game)
            .await?
            .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "not found"))?;
        (row, notices::board_in(&mut conn, &game, lang).await?)
    };
    let mut items: Vec<Notice> = maintenance_notice(&row, lang, fi_domain::UnixMs::now_system())
        .into_iter()
        .collect();
    items.extend(board.iter().map(notice));
    Ok(([(CACHE_CONTROL, NOTICES_CACHE)], Json(NoticeList { items })))
}

/// `?game=` of the summary.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryQuery {
    /// Game (required).
    pub game: Option<String>,
}

/// `GET /core/v1/summary?game=fh2` → 200 [`ServerSummary`]: the live `onlineNow` (the same count
/// as the status' `queue.*.active`) and the game's last nightly snapshot (`fi_db::summary`, `null`
/// before the first run). `PORTAL.NOT_FOUND` for a game this deployment does not serve.
pub async fn summary(
    State(state): State<AppState>,
    query: Result<Query<SummaryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<
    (
        [(axum::http::HeaderName, &'static str); 1],
        Json<ServerSummary>,
    ),
    ApiError,
> {
    let Query(q) = query.map_err(|_| ApiError::invalid("query", "game only"))?;
    let game = q
        .game
        .filter(|g| crate::valid_game(g))
        .ok_or_else(|| ApiError::invalid("game", "a game id such as fh2"))?;
    let (queue, nightly) = {
        let mut conn = state.pool.acquire().await?;
        notices::game_in(&mut conn, &game)
            .await?
            .ok_or_else(|| ApiError::new(ErrorCode::PortalNotFound, "not found"))?;
        (
            notices::queue_summary_in(&mut conn).await?,
            fi_db::summary::latest_in(&mut conn, &game).await?,
        )
    };
    let online_now = queue
        .iter()
        .filter(|q| q.game == game)
        .map(|q| q.active.max(0) as u64)
        .sum();
    Ok((
        [(CACHE_CONTROL, SUMMARY_CACHE)],
        Json(ServerSummary {
            game,
            generated_at: views::at(fi_domain::UnixMs::now_system()),
            online_now,
            nightly,
        }),
    ))
}
