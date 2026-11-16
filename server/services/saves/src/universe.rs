//! `/saves/v1` (UNIVERSE_API.md §2-§3): nearby and the additive progress fact, behind the game
//! access ticket ([`Ticket`]: any pool on this deployment and a served game, with or without a
//! save line, OD-239). `nearby` goes through the instance's read cache ([`Caches`]: positive
//! answers only, keyed by the full query and the caller) before one indexed query
//! (`fi_db::universe`); every miss counts one query. The progress fact is one transaction: the
//! fact (idempotent by `X-Fh2-Request-Id`), then a `race_result`'s CR-L14 `min_time` check,
//! answered `201 applied: false` and a signal. The leaderboards, rivals and ghosts of this API (and
//! the `score` and `ghost` progress kinds) were retired on 2026-10-27: the game's own are the
//! Forza RPC's (GAME_PLATFORM.md §3a).

use crate::Universe;
use axum::Router;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use fi_db::game::{self as dbgame, Fact};
use fi_db::universe::{self as db, Applied, Writer};
use fi_domain::UnixMs;
use fi_protocol::routes::universe as r;
use fi_protocol::universe::{
    MAX_COORDINATE, MAX_NEARBY_LIMIT, MAX_NEARBY_RADIUS, Nearby, NearbyItem, NearbyQuery,
    PROGRESS_KINDS, PositionData, ProgressFact, ProgressRecorded, ProgressSignal, RaceResultData,
};
use fi_protocol::{ErrorCode, headers};
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::Body;
use fi_surfaces::game::cache::TtlCache;
use fi_surfaces::game::event_id;
use fi_surfaces::game::ticket::Ticket;
use fi_surfaces::state::AppState;
use fi_surfaces::views;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// The read cache of one instance, with its query counter.
#[derive(Debug)]
pub struct Caches {
    /// Nearby answers.
    pub nearby: TtlCache<String, Nearby>,
    /// Database reads made for it (a cache hit makes none).
    pub queries: AtomicU64,
}

impl Caches {
    /// The cache with this TTL, 4096 answers.
    pub fn new(ttls: crate::config::CacheTtls) -> Self {
        Self {
            nearby: TtlCache::new(ttls.nearby, 4096),
            queries: AtomicU64::new(0),
        }
    }

    fn query(&self) {
        self.queries.fetch_add(1, Ordering::Relaxed);
    }

    /// `(endpoint, hits, misses)` of every cache (the periodic metrics line).
    pub fn counts(&self) -> [(&'static str, u64, u64); 1] {
        let (hits, misses) = self.nearby.counts();
        [("nearby", hits, misses)]
    }
}

/// The `/saves/v1` routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::NEARBY, get(nearby))
        .route(r::PROGRESS, post(progress))
}

const PRIVATE_NO_STORE: &str = "private, no-store";

fn with_cache_control<T: serde::Serialize>(body: &T, value: &'static str) -> Response {
    let mut response = axum::Json(body).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    response
}

fn query<T: serde::de::DeserializeOwned>(
    q: Result<Query<T>, axum::extract::rejection::QueryRejection>,
) -> Result<T, ApiError> {
    q.map(|Query(v)| v)
        .map_err(|_| ApiError::invalid("query", "unknown or malformed query parameter"))
}

fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b':' | b'-'))
}

/// The caller's mode; an explicit other mode is `PORTAL.MODE_MISMATCH`.
fn mode_of(ticket: &Ticket, asked: Option<&str>) -> Result<String, ApiError> {
    match asked {
        Some(m) if m != ticket.claims.mode => Err(ApiError::new(
            ErrorCode::PortalModeMismatch,
            "Hardcore and Azimuth never see each other's universe",
        )),
        _ => Ok(ticket.claims.mode.clone()),
    }
}

fn limit(asked: Option<u32>, default: u32, max: u32, field: &'static str) -> Result<u32, ApiError> {
    let n = asked.unwrap_or(default);
    if !(1..=max).contains(&n) {
        return Err(ApiError::invalid(field, "out of range"));
    }
    Ok(n)
}

/// `GET /saves/v1/nearby`.
async fn nearby(
    State(state): State<AppState>,
    Extension(u): Extension<Universe>,
    ticket: Ticket,
    q: Result<Query<NearbyQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let q = query(q)?;
    let mode = mode_of(&ticket, q.mode.as_deref())?;
    if q.x.abs() > MAX_COORDINATE || q.y.abs() > MAX_COORDINATE {
        return Err(ApiError::invalid("x", "a map coordinate"));
    }
    if !(1..=MAX_NEARBY_RADIUS).contains(&q.radius) {
        return Err(ApiError::invalid("radius", "1 to 5000"));
    }
    let n = limit(q.limit, 16, MAX_NEARBY_LIMIT, "limit")?;
    let game = ticket.game().to_owned();
    let key = format!(
        "{game}|{mode}|{}|{}|{}|{n}|{}",
        q.x, q.y, q.radius, ticket.live.profile_id
    );
    if let Some(answer) = u.caches.nearby.get(&key) {
        return Ok(with_cache_control(&answer, PRIVATE_NO_STORE));
    }
    u.caches.query();
    let rows = db::nearby(
        &state.pool,
        &game,
        &mode,
        q.x,
        q.y,
        i64::from(q.radius),
        ticket.live.profile_id,
        n,
    )
    .await?;
    let answer = Nearby {
        items: rows
            .iter()
            .map(|r| NearbyItem {
                xuid: r.xuid.to_string(),
                gamertag: r.gamertag.clone(),
                mode: mode.clone(),
                x: i64::from(r.x),
                y: i64::from(r.y),
                distance: r.distance.round() as u32,
                car: r.car.and_then(|c| u32::try_from(c).ok()),
                updated_at: views::at(r.updated_at),
            })
            .collect(),
    };
    u.caches.nearby.put(key, answer.clone());
    Ok(with_cache_control(&answer, PRIVATE_NO_STORE))
}

fn data_field<T: serde::de::DeserializeOwned>(
    data: &serde_json::Map<String, serde_json::Value>,
) -> Result<T, ApiError> {
    serde_json::from_value(serde_json::Value::Object(data.clone()))
        .map_err(|_| ApiError::invalid("data", "malformed data for this kind"))
}

/// A `race_result`'s event and time, checked before the transaction.
fn race_of(fact: &ProgressFact) -> Result<Option<RaceResultData>, ApiError> {
    if fact.kind != "race_result" {
        return Ok(None);
    }
    let d: RaceResultData = data_field(&fact.data)?;
    if !valid_id(&d.event) || d.time_ms <= 0 {
        return Err(ApiError::invalid(
            "data.timeMs",
            "an event and a positive time",
        ));
    }
    Ok(Some(d))
}

/// `POST /saves/v1/progress`.
async fn progress(
    State(state): State<AppState>,
    ticket: Ticket,
    request_headers: HeaderMap,
    Body(fact): Body<ProgressFact>,
) -> Result<Response, ApiError> {
    let request_id = request_headers
        .get(headers::FH2_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v.trim()).ok())
        .filter(|u| !u.is_nil())
        .ok_or_else(|| ApiError::invalid("X-Fh2-Request-Id", "a UUID request id is required"))?;
    if !PROGRESS_KINDS.contains(&fact.kind.as_str()) {
        return Err(ApiError::invalid("kind", "unknown progress kind"));
    }
    let id = event_id(ticket.live.profile_id, request_id);
    let writer = Writer {
        game: ticket.game().to_owned(),
        mode: ticket.claims.mode.clone(),
        profile_id: ticket.live.profile_id,
        session_id: ticket.live.session_id,
    };
    if fact.kind == "position" {
        let d: PositionData = data_field(&fact.data)?;
        let coordinate = |v: i64| (v.abs() <= MAX_COORDINATE).then_some(v as i32);
        let (Some(x), Some(y)) = (coordinate(d.x), coordinate(d.y)) else {
            return Err(ApiError::invalid("data.x", "a map coordinate"));
        };
        if d.heading.is_some_and(|h| h > 359) {
            return Err(ApiError::invalid("data.heading", "0 to 359"));
        }
        let mut tx = state.pool.begin().await?;
        let fresh = db::position_in(
            &mut tx,
            &writer,
            id,
            x,
            y,
            d.heading.map(|h| h as i16),
            d.car.and_then(|c| i32::try_from(c).ok()),
        )
        .await?;
        tx.commit().await?;
        return Ok(answer(id, fresh, UnixMs::now_system(), Applied::Applied));
    }
    let race = race_of(&fact)?;
    // OD-112 follow-up: a modded session's race results answer as an accepted fact and write
    // nothing (no fact, no refusal row).
    if race.is_some() && fi_surfaces::game::modded_result_skipped(&ticket.live, "progress") {
        return Ok(answer(id, true, UnixMs::now_system(), Applied::Applied));
    }
    let mut tx = state.pool.begin().await?;
    let recorded = dbgame::record_progression_in(
        &mut tx,
        &Fact {
            event_id: id,
            account_id: ticket.live.account_id,
            profile_id: ticket.live.profile_id,
            session_id: Some(ticket.live.session_id),
            kind: fact.kind.clone(),
            data: fact.data.clone(),
        },
    )
    .await?;
    let applied = match (&race, recorded.recorded) {
        // A retry: nothing written.
        (_, false) => Applied::Refused("", ""),
        (Some(d), true) => db::race_time_in(&mut tx, &writer, &d.event, d.time_ms, id).await?,
        (None, true) => Applied::Applied,
    };
    tx.commit().await?;
    Ok(answer(id, recorded.recorded, recorded.recorded_at, applied))
}

fn answer(id: Uuid, recorded: bool, at: UnixMs, applied: Applied) -> Response {
    let (applied, signal) = match (recorded, applied) {
        (false, _) => (false, None),
        (true, Applied::Applied) => (true, None),
        (true, Applied::Refused(rule, field)) => (
            false,
            Some(ProgressSignal {
                rule: rule.to_owned(),
                field: field.to_owned(),
            }),
        ),
    };
    let status = if recorded {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut response = (
        status,
        axum::Json(ProgressRecorded {
            event_id: id.to_string(),
            recorded,
            recorded_at: views::at(at),
            applied,
            signal,
        }),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The periodic metrics line (cache hits and misses per endpoint, queries).
pub fn log_metrics(caches: &Caches) {
    for (endpoint, hits, misses) in caches.counts() {
        tracing::info!(
            endpoint,
            cache_hit = hits,
            cache_miss = misses,
            "universe cache"
        );
    }
    tracing::info!(
        queries = caches.queries.load(Ordering::Relaxed),
        "universe reads"
    );
}

/// How often the background task flushes the liveness batch.
pub const FLUSH_EVERY: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_short_and_plain() {
        assert!(valid_id("e.12:sprint-3") && !valid_id("-e") && !valid_id("a b"));
        assert!(!valid_id("") && !valid_id(&"a".repeat(65)));
    }
}
