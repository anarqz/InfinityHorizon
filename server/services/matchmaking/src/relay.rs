//! The relay → platform routes of the anti-cheat detector (MATCHMAKING_API.md §5d, register CR-S80):
//! `POST /mm/v1/relay/watch` (the current `WatchRules`) and `POST /mm/v1/relay/reports` (a batch of
//! detector reports). Outside §1's ticket rule: a relay is authenticated only by its signature and
//! its node key's pin in `FI_RELAYS`.
//!
//! Order: the body (at most 64 KiB, else 413; strict JSON, else 400 `REQUEST.INVALID {field}`), the
//! signature (any failure 401 with an empty body: no oracle), the per-node-key bucket (429 with
//! `Retry-After`), the fields (400), then each report in its own transaction (`fi_db::relay_reports`,
//! never a transaction across a call). The answer is signed by this instance's session key, whose
//! certificate (by the relay-grant key the relay pins) rides in every answer.
//!
//! The per-instance session key is derived state (ADR 0008 amendment pattern): generated in memory
//! at start and every 24 h, never stored, its loss costs nothing (the next answer carries the new
//! certificate). Logs carry counts, outcomes and the relay's name, never a key, an id or a body.

use crate::Mm;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use ed25519_dalek::SigningKey;
use fi_db::limits::{self, Hit, Limit};
use fi_db::relay_reports::{self as reports, NewRelayReport, Stored};
use fi_netwatch::cert;
use fi_protocol::ErrorCode;
use fi_protocol::matchmaking::{WatchJump, WatchLimit, WatchRules, WatchSpeed};
use fi_protocol::relay_watch::{
    CLOCK_SKEW_SECS, MAX_REPORTS, MM_REPORT_ANSWER_PREFIX, MM_WATCH_ANSWER_PREFIX, MmSessionCert,
    RELAY_REPORT_PREFIX, RELAY_WATCH_PREFIX, RelayReportAnswer, RelayReportBatch, RelaySessionCert,
    RelayWatch, RelayWatchRequest, SIGNATURE_HEADER,
};
use fi_protocol::routes::mm as r;
use fi_surfaces::errors::ApiError;
use fi_surfaces::state::{AppState, now_secs};
use std::sync::{Arc, RwLock};
use uuid::Uuid;

/// Report batches per node key (`mm.relay.report`, 60 a minute).
pub const REPORT_LIMIT: Limit = Limit {
    name: "mm.relay.report",
    max: 60,
    window_s: 60,
};
/// Rules fetches per node key (`mm.relay.watch`, 12 a minute).
pub const WATCH_LIMIT: Limit = Limit {
    name: "mm.relay.watch",
    max: 12,
    window_s: 60,
};

/// The routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(r::RELAY_WATCH, post(watch))
        .route(r::RELAY_REPORTS, post(reports_batch))
}

/// This instance's session: its ephemeral key and the certificate the relay-grant key signed.
pub struct Session {
    key: SigningKey,
    cert: MmSessionCert,
    made_at: i64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("kid", &self.cert.kid)
            .finish_non_exhaustive()
    }
}

/// The current [`Session`], re-keyed every 24 h.
#[derive(Debug)]
pub struct Sessions {
    grant: SigningKey,
    kid: String,
    current: RwLock<Arc<Session>>,
}

fn fresh(grant: &SigningKey, kid: &str, now: i64) -> Session {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("the system random source is unavailable");
    let key = SigningKey::from_bytes(&seed);
    let instance = Uuid::now_v7().to_string();
    let cert = cert::mm_cert(kid, grant, &key.verifying_key(), &instance, now);
    Session {
        key,
        cert,
        made_at: now,
    }
}

impl Sessions {
    /// A session certified by the relay-grant ring's current key.
    pub fn new(ring: &fi_auth::keys::SigningKeys) -> Self {
        let (kid, grant) = ring.current();
        let now = now_secs();
        Self {
            grant: grant.clone(),
            kid: kid.to_owned(),
            current: RwLock::new(Arc::new(fresh(grant, kid, now))),
        }
    }

    /// The session now (a new one after [`cert::ROTATE_SECS`]).
    pub fn get(&self) -> Arc<Session> {
        let now = now_secs();
        if let Ok(s) = self.current.read()
            && now - s.made_at < cert::ROTATE_SECS
        {
            return s.clone();
        }
        let next = Arc::new(fresh(&self.grant, &self.kid, now));
        if let Ok(mut w) = self.current.write() {
            *w = next.clone();
        }
        next
    }
}

fn unauthorized() -> Response {
    StatusCode::UNAUTHORIZED.into_response()
}

/// Checks a relay request's certificate and signature (§5d "Platform verifies"); the node key.
#[allow(clippy::too_many_arguments)]
fn verify(
    mm: &crate::MmState,
    headers: &HeaderMap,
    prefix: &[u8],
    body: &[u8],
    c: &RelaySessionCert,
    relay: &str,
    start_id: &str,
    iat: i64,
) -> Option<String> {
    let now = now_secs();
    let session = cert::verify_relay_cert(c, |k| mm.relay(k).is_some(), now).ok()?;
    if relay != c.node_key || start_id != c.start_id {
        return None;
    }
    let sig = headers.get(SIGNATURE_HEADER)?.to_str().ok()?;
    cert::verify_body(&session, prefix, body, sig).ok()?;
    ((iat - now).abs() <= CLOCK_SKEW_SECS).then(|| c.node_key.clone())
}

async fn limited(state: &AppState, limit: &Limit, node_key: &str) -> Result<Option<u64>, ApiError> {
    use std::fmt::Write;
    let digest = <sha2::Sha256 as sha2::Digest>::digest(format!("relay:{node_key}").as_bytes());
    let mut key = format!("{}:", limit.name);
    for b in digest {
        let _ = write!(key, "{b:02x}");
    }
    Ok(match limits::hit(&state.pool, &key, limit).await? {
        Hit::Allowed => None,
        Hit::Limited { retry_after_secs } => Some(u64::try_from(retry_after_secs).unwrap_or(1)),
    })
}

fn rate_limited(secs: u64) -> ApiError {
    ApiError::new(ErrorCode::RequestRateLimited, "too many requests").retry_after(secs)
}

/// A signed JSON answer: the exact bytes the signature covers.
fn signed(session: &Session, prefix: &[u8], bytes: Vec<u8>) -> Response {
    let sig = cert::sign_body(&session.key, prefix, &bytes);
    let mut response = (StatusCode::OK, bytes).into_response();
    let h = response.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&sig) {
        h.insert(SIGNATURE_HEADER, v);
    }
    response
}

/// The current `WatchRules` (the object `GET /mm/v1/watch` serves).
pub fn current_rules() -> WatchRules {
    let r = fi_db::observations::current();
    WatchRules {
        version: r.version,
        speed: WatchSpeed {
            max_kmh: r.speed_kmh,
            sustain_ms: r.speed_ms,
            per_car_floor_kmh: fi_db::observations::per_car_floor_kmh(r),
        },
        climb: WatchLimit {
            max_kmh: r.climb_kmh,
            sustain_ms: r.climb_ms,
        },
        jump: WatchJump {
            max_meters: r.jump_m,
        },
    }
}

/// `POST /mm/v1/relay/watch` (§5d "The rules").
async fn watch(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let req: RelayWatchRequest = fi_surfaces::extract::parse(&body)?;
    let Some(node) = verify(
        &mm,
        &headers,
        RELAY_WATCH_PREFIX,
        &body,
        &req.cert,
        &req.relay,
        &req.start_id,
        req.iat,
    ) else {
        return Ok(unauthorized());
    };
    if let Some(secs) = limited(&state, &WATCH_LIMIT, &node).await? {
        return Err(rate_limited(secs));
    }
    if Uuid::parse_str(&req.nonce).is_err() {
        return Err(ApiError::invalid("nonce", "a UUID"));
    }
    let session = mm.relay_sessions.get();
    let answer = RelayWatch {
        nonce: req.nonce,
        rules: current_rules(),
        cert: session.cert.clone(),
    };
    let bytes = serde_json::to_vec(&answer).unwrap_or_default();
    Ok(signed(&session, MM_WATCH_ANSWER_PREFIX, bytes))
}

fn report_input(
    node: &str,
    start_id: Uuid,
    r: fi_protocol::relay_watch::RelayReport,
) -> Result<NewRelayReport, ApiError> {
    let uuid = |v: &str, field: &'static str| {
        Uuid::parse_str(v).map_err(|_| ApiError::invalid(format!("reports.{field}"), "a UUID"))
    };
    let xuid = |v: &str, field: &'static str| {
        v.parse::<i64>()
            .ok()
            .filter(|x| *x > 0)
            .ok_or_else(|| ApiError::invalid(format!("reports.{field}"), "a XUID, decimal"))
    };
    let input = NewRelayReport {
        id: uuid(&r.id, "id")?,
        episode: uuid(&r.episode, "episode")?,
        relay_key: node.to_owned(),
        start_id,
        world_id: uuid(&r.world, "world")?,
        xuid: xuid(&r.xuid, "xuid")?,
        host_xuid: xuid(&r.host_xuid, "hostXuid")?,
        kind_type: r.kind_type,
        kind: r.kind,
        from_ms: r.from,
        to_ms: r.to,
        rules_version: r.rules_version,
        car_id: r.car_id,
        peak_kmh: r.peak_kmh,
        over_limit_ms: r.over_limit_ms,
        own_peak_kmh: r.own_peak_kmh,
        legs: r.legs,
        max_jump_m: r.max_jump_m,
    };
    reports::validate(&input).map_err(|field| {
        ApiError::invalid(format!("reports.{field}"), "see MATCHMAKING_API.md §5d")
    })?;
    Ok(input)
}

/// `POST /mm/v1/relay/reports` (§5d "Reports").
async fn reports_batch(
    State(state): State<AppState>,
    Extension(mm): Extension<Mm>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let batch: RelayReportBatch = fi_surfaces::extract::parse(&body)?;
    let Some(node) = verify(
        &mm,
        &headers,
        RELAY_REPORT_PREFIX,
        &body,
        &batch.cert,
        &batch.relay,
        &batch.start_id,
        batch.iat,
    ) else {
        return Ok(unauthorized());
    };
    if let Some(secs) = limited(&state, &REPORT_LIMIT, &node).await? {
        return Err(rate_limited(secs));
    }
    let start_id =
        Uuid::parse_str(&batch.start_id).map_err(|_| ApiError::invalid("startId", "a UUID"))?;
    if batch.reports.len() > MAX_REPORTS {
        return Err(ApiError::invalid("reports", "at most 64"));
    }
    let c = &batch.counters;
    let counters = reports::Counters {
        decoded: c.decoded,
        undecodable: c.undecodable,
        foreign_owner: c.foreign_owner,
        xuid_mismatch: c.xuid_mismatch,
        witness_full: c.witness_full,
        rules_missing: c.rules_missing,
        rule_unevaluable: c.rule_unevaluable,
        report_dropped: c.report_dropped,
        rings_in_use: c.rings_in_use,
    };
    if [
        counters.decoded,
        counters.undecodable,
        counters.foreign_owner,
        counters.xuid_mismatch,
        counters.witness_full,
        counters.rules_missing,
        counters.rule_unevaluable,
        counters.report_dropped,
        counters.rings_in_use,
    ]
    .iter()
    .any(|v| *v < 0)
    {
        return Err(ApiError::invalid("counters", "non-negative counts"));
    }
    let inputs = batch
        .reports
        .into_iter()
        .map(|r| report_input(&node, start_id, r))
        .collect::<Result<Vec<_>, _>>()?;
    let mut accepted = Vec::with_capacity(inputs.len());
    let (mut repeats, mut would_flag, mut recorded, mut rejected) = (0u32, 0u32, 0u32, 0u32);
    for input in &inputs {
        match reports::store(&state.pool, input).await? {
            Stored::Repeat => repeats += 1,
            Stored::New { outcome, .. } => match outcome {
                "rejected" => rejected += 1,
                "recorded" => recorded += 1,
                _ => would_flag += 1,
            },
        }
        accepted.push(input.id.to_string());
    }
    reports::add_counters(&state.pool, &node, start_id, &counters).await?;
    if !inputs.is_empty() {
        let relay = mm.relay(&node).map(|r| r.name.as_str()).unwrap_or("?");
        tracing::info!(
            event = "relay_reports",
            relay,
            reports = inputs.len(),
            repeats,
            would_flag,
            recorded,
            rejected,
            record_only = reports::RELAY_REPORTS_RECORD_ONLY,
            "relay report batch"
        );
    }
    let session = mm.relay_sessions.get();
    let answer = RelayReportAnswer {
        seq: batch.seq,
        start_id: batch.start_id.clone(),
        accepted,
        rules_version: fi_db::observations::current().version,
        cert: session.cert.clone(),
    };
    let bytes = serde_json::to_vec(&answer).unwrap_or_default();
    Ok(signed(&session, MM_REPORT_ANSWER_PREFIX, bytes))
}
