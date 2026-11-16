//! HTTP plumbing shared by the services: request context, health and meta routes, serving.
//!
//! Every request runs inside a `request` span carrying `request_id` (the caller's `X-Request-Id`,
//! or a new UUIDv7) and, when the request comes with one, the trace id of W3C `traceparent` or
//! Google's `X-Cloud-Trace-Context` (Cloud Run adds the latter to every request), so each log
//! line of the request links to its trace (crate::telemetry). Each request ends with one access-log
//! line (DEBUG when it succeeded fast or was an expected refusal, WARN or ERROR otherwise); the
//! request id is echoed in the response.

use crate::config::{self, ConfigError, ServiceInfo};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use std::future::Future;
use std::net::IpAddr;
use std::time::Instant;
use tracing::{Instrument, field};

/// Request id header, read from the caller and echoed in the response.
pub const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");
/// W3C trace context.
pub const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");
/// Google's legacy trace header: `TRACE_ID/SPAN_ID;o=1`, the span id in decimal.
pub const CLOUD_TRACE: HeaderName = HeaderName::from_static("x-cloud-trace-context");
/// A request at least this slow is logged at WARN even when it succeeded.
pub const SLOW_REQUEST_MS: f64 = 1000.0;

/// Trace ids of the request, as Cloud Logging expects them (32 and 16 lower-case hex digits).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceContext {
    /// 32 hex digits.
    pub trace_id: String,
    /// 16 hex digits.
    pub span_id: String,
    /// Whether the caller sampled the trace.
    pub sampled: bool,
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit()) && s.bytes().any(|b| b != b'0')
}

/// Parses W3C `traceparent`: `00-<trace id>-<span id>-<flags>`.
pub fn parse_traceparent(value: &str) -> Option<TraceContext> {
    let mut parts = value.trim().split('-');
    let (version, trace, span, flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if version != "00"
        || !is_hex(trace, 32)
        || !is_hex(span, 16)
        || flags.len() != 2
        || parts.next().is_some()
    {
        return None;
    }
    let flags = u8::from_str_radix(flags, 16).ok()?;
    Some(TraceContext {
        trace_id: trace.to_ascii_lowercase(),
        span_id: span.to_ascii_lowercase(),
        sampled: flags & 1 == 1,
    })
}

/// Parses `X-Cloud-Trace-Context`: `<trace id>/<decimal span id>;o=<0|1>`.
pub fn parse_cloud_trace(value: &str) -> Option<TraceContext> {
    let (trace, rest) = value.trim().split_once('/')?;
    let (span, options) = match rest.split_once(';') {
        Some((span, options)) => (span, Some(options)),
        None => (rest, None),
    };
    if !is_hex(trace, 32) {
        return None;
    }
    let span: u64 = span.parse().ok().filter(|s| *s != 0)?;
    Some(TraceContext {
        trace_id: trace.to_ascii_lowercase(),
        span_id: format!("{span:016x}"),
        sampled: options == Some("o=1"),
    })
}

/// The trace context of a request, W3C first.
pub fn trace_context(headers: &HeaderMap) -> Option<TraceContext> {
    let text = |name: &HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    text(&TRACEPARENT)
        .and_then(parse_traceparent)
        .or_else(|| text(&CLOUD_TRACE).and_then(parse_cloud_trace))
}

/// A caller's request id is kept only when it is short printable ASCII; anything else is replaced.
fn request_id(headers: &HeaderMap) -> String {
    headers
        .get(&REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| (1..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string())
}

/// What a refusal was, for the access-log line: the stable error code and, for a refused
/// request, the name of the field it names (`details.field`; a name, never a value). The service
/// that renders the refusal inserts it as a response extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The error code (`REQUEST.INVALID`).
    pub code: &'static str,
    /// `details.field`, cut to 64 characters.
    pub field: Option<String>,
}

/// Middleware: the request span, the access-log line and the echoed request id.
pub async fn request_context(request: Request, next: Next) -> Response {
    let id = request_id(request.headers());
    let trace = trace_context(request.headers());
    let span = tracing::info_span!(
        "request",
        request_id = %id,
        method = %request.method(),
        path = %log_path(request.uri().path()),
        trace_id = field::Empty,
        span_id = field::Empty,
        trace_sampled = field::Empty,
    );
    if let Some(t) = &trace {
        span.record("trace_id", t.trace_id.as_str());
        span.record("span_id", t.span_id.as_str());
        span.record("trace_sampled", t.sampled);
    }
    let started = Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID, value);
    }
    let status = response.status().as_u16();
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    // At game-plane rates (hundreds of polling requests per second) one INFO line per request would
    // cost more in Cloud Logging than the service itself (ADR 0010 §3): successful, fast requests log
    // at DEBUG, and so do the refusals clients handle by design (401: an expired access or session
    // token, refreshed by the launcher; 404); other refusals and slow requests at WARN, failures at
    // ERROR. Request counts and latencies come from Cloud Run's metrics, which log exclusions do not
    // affect.
    span.in_scope(|| {
        if status >= 500 {
            tracing::error!(status, latency_ms, "request failed");
        } else if (status >= 400 && !quiet_refusal(status)) || latency_ms >= SLOW_REQUEST_MS {
            let refusal = response.extensions().get::<Refusal>();
            tracing::warn!(
                status,
                latency_ms,
                code = refusal.map(|r| r.code),
                field = refusal.and_then(|r| r.field.as_deref()),
                "request refused or slow"
            );
        } else {
            tracing::debug!(status, latency_ms, "request served");
        }
    });
    response
}

/// The request path as logged: every run of 13 or more ASCII digits becomes `…`. A path can name a
/// player (the game plane's `/users/xuid(N)/…`, `/launcher/v1/profiles/{xuid}/…`; XUIDs and the
/// drivatars' ids are 13 to 20 digits) and logs never carry XUIDs; shorter numbers (a title id, a
/// UUID's groups) stay readable (review of OD-44, server/REVIEW_LOG.md).
pub fn log_path(path: &str) -> String {
    const MIN_RUN: usize = 13;
    let mut out = String::with_capacity(path.len());
    let mut digits = String::new();
    for c in path.chars().chain(std::iter::once('/')) {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        if digits.len() >= MIN_RUN {
            out.push('…');
        } else {
            out.push_str(&digits);
        }
        digits.clear();
        out.push(c);
    }
    out.pop();
    out
}

/// Refusals clients handle by design, logged at DEBUG: 401 (refresh the token and retry; the game
/// plane's answer to an expired or missing session token, which the hook turns into the game's
/// offline mode) and 404.
pub const fn quiet_refusal(status: u16) -> bool {
    matches!(status, 401 | 404)
}

/// The header the edge (a Cloudflare transform rule) adds to every proxied request (ADR 0903 §1).
pub const EDGE_HEADER: HeaderName = HeaderName::from_static("x-fi-edge");

/// Shortest accepted edge secret.
pub const EDGE_SECRET_MIN_LEN: usize = 32;

/// The origin lock (ADR 0903 §1): `FI_EDGE_SECRET`, one value, or two separated by a comma while
/// the secret rotates (the new one first). Never printed.
#[derive(Clone)]
pub struct EdgeSecret {
    values: Vec<Vec<u8>>,
}

impl std::fmt::Debug for EdgeSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EdgeSecret({} value(s), ***)", self.values.len())
    }
}

/// A request that came through the edge (the extension [`edge_guard`] sets when the secret
/// matched): only then is `CF-Connecting-IP` the client's address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeVerified;

/// Constant-time equality (the length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

impl EdgeSecret {
    /// Parses `one` or `new,old`; each value printable ASCII of at least
    /// [`EDGE_SECRET_MIN_LEN`] characters.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let values: Vec<Vec<u8>> = text
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.as_bytes().to_vec())
            .collect();
        let invalid = |reason: &str| ConfigError::Invalid {
            name: "FI_EDGE_SECRET",
            reason: reason.to_owned(),
        };
        if values.is_empty() || values.len() > 2 {
            return Err(invalid("one value, or two during a rotation"));
        }
        if values
            .iter()
            .any(|v| v.len() < EDGE_SECRET_MIN_LEN || !v.iter().all(u8::is_ascii_graphic))
        {
            return Err(invalid("at least 32 printable characters per value"));
        }
        Ok(Self { values })
    }

    /// `FI_EDGE_SECRET` when set (`None`: no origin lock, the local stack).
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        config::optional("FI_EDGE_SECRET")
            .map(|v| Self::parse(&v))
            .transpose()
    }

    /// Whether `presented` equals one of the values (every value is compared).
    pub fn matches(&self, presented: &[u8]) -> bool {
        self.values
            .iter()
            .fold(false, |found, v| ct_eq(presented, v) | found)
    }
}

/// An IPv4 or IPv6 network (`a.b.c.d/n`, `x::/n`, or a bare address: one host).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parses `addr[/prefix]` (host bits are ignored); `None` when it is not one.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (text.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Self { addr, prefix })
    }

    /// Whether `ip` (an IPv4-mapped IPv6 address counts as its IPv4 address) is inside.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let bits = u32::from(self.prefix);
        match (self.addr, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// Where a request's client address comes from when the request did not come through the edge
/// (`FI_CLIENT_ADDRESS`; ADR 0903 §1, ACCOUNTS.md §9). An [`EdgeVerified`] request always uses
/// `CF-Connecting-IP`, whatever this says. [`serve`] reads it once at startup and puts it in every
/// request as `Extension<Arc<ClientAddress>>`; a router served in process (tests) has none, which
/// readers treat as [`ClientAddress::Forwarded`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientAddress {
    /// `forwarded`: the rightmost `X-Forwarded-For` entry, else the peer. Right only behind a
    /// front end that appends the entry (Google's; the local stack and its simulator): a client
    /// that reaches the process directly chooses it. The default outside prod.
    Forwarded,
    /// `peer`: the TCP peer only; `X-Forwarded-For` and `CF-Connecting-IP` are ignored. The default
    /// in prod (where every request but `/health/*` is edge-verified anyway).
    Peer,
    /// `trusted-proxy`: from a peer inside `FI_TRUSTED_PROXIES`, `CF-Connecting-IP`, else the
    /// rightmost `X-Forwarded-For` entry, else the peer; from any other peer, the peer.
    TrustedProxy(Vec<Cidr>),
}

impl ClientAddress {
    /// Reads `FI_CLIENT_ADDRESS` and `FI_TRUSTED_PROXIES` through `get` (unset and blank are the
    /// same). Unset: `forwarded`, or `peer` when `APP_ENV` or `FI_ENV` is prod. An unknown mode, a
    /// malformed network, or `trusted-proxy` without a network is an error.
    pub fn parse(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |name: &str| get(name).filter(|v| !v.trim().is_empty());
        let prod = ["APP_ENV", "FI_ENV"]
            .iter()
            .any(|n| get(n).is_some_and(|v| v.trim().eq_ignore_ascii_case("prod")));
        let proxies = match get("FI_TRUSTED_PROXIES") {
            None => Vec::new(),
            Some(list) => list
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(Cidr::parse)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| ConfigError::Invalid {
                    name: "FI_TRUSTED_PROXIES",
                    reason: "a comma-separated list of IP networks (a.b.c.d/n, x::/n)".to_owned(),
                })?,
        };
        let mode = get("FI_CLIENT_ADDRESS").map(|v| v.trim().to_ascii_lowercase());
        match mode.as_deref() {
            None if prod => Ok(Self::Peer),
            None | Some("forwarded") => Ok(Self::Forwarded),
            Some("peer") => Ok(Self::Peer),
            Some("trusted-proxy") if proxies.is_empty() => {
                Err(ConfigError::Missing("FI_TRUSTED_PROXIES"))
            }
            Some("trusted-proxy") => Ok(Self::TrustedProxy(proxies)),
            Some(_) => Err(ConfigError::Invalid {
                name: "FI_CLIENT_ADDRESS",
                reason: "forwarded, peer or trusted-proxy".to_owned(),
            }),
        }
    }

    /// [`Self::parse`] over the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::parse(&|name| std::env::var(name).ok())
    }

    /// The setting's name (`forwarded`, `peer`, `trusted-proxy`), for the startup log.
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Forwarded => "forwarded",
            Self::Peer => "peer",
            Self::TrustedProxy(_) => "trusted-proxy",
        }
    }

    /// Whether a request from `peer` may name its client in headers.
    pub fn trusts(&self, peer: IpAddr) -> bool {
        match self {
            Self::Forwarded => true,
            Self::Peer => false,
            Self::TrustedProxy(nets) => nets.iter().any(|n| n.contains(peer)),
        }
    }
}

/// Whether `path` is a health probe (never behind the edge lock or a rate limit).
pub fn is_health(path: &str) -> bool {
    path.starts_with("/health/")
}

/// Middleware of the origin lock: with a secret, every request but `/health/*` must carry
/// [`EDGE_HEADER`] with a matching value, else 403 `AUTH.FORBIDDEN`; the header is removed before
/// the handler runs and the request gets [`EdgeVerified`]. Without a secret it passes everything
/// and marks nothing.
pub async fn edge_guard(
    axum::extract::State(secret): axum::extract::State<Option<std::sync::Arc<EdgeSecret>>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(secret) = secret else {
        return next.run(request).await;
    };
    if is_health(request.uri().path()) {
        return next.run(request).await;
    }
    let ok = request
        .headers()
        .get(&EDGE_HEADER)
        .is_some_and(|v| secret.matches(v.as_bytes()));
    if !ok {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "code": "AUTH.FORBIDDEN",
                "message": "this origin answers requests from the edge only"
            })),
        )
            .into_response();
    }
    request.headers_mut().remove(&EDGE_HEADER);
    request.extensions_mut().insert(EdgeVerified);
    next.run(request).await
}

/// `/health/live` (the process answers) and `/health/ready` (its dependencies answer; Cloud Run's
/// startup probe uses it, so a revision that cannot reach Postgres never takes traffic).
pub fn health_router<F, Fut>(ready: F) -> Router
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route(
            "/health/ready",
            get(move || {
                let ready = ready.clone();
                async move {
                    match ready().await {
                        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
                        Err(reason) => {
                            tracing::warn!(%reason, "not ready");
                            (
                                StatusCode::SERVICE_UNAVAILABLE,
                                Json(json!({ "status": "unavailable", "reason": reason })),
                            )
                                .into_response()
                        }
                    }
                }
            }),
        )
}

/// `GET /meta`: which service, version, revision and configuration answered.
pub fn meta_router(info: ServiceInfo) -> Router {
    Router::new().route("/meta", get(move || std::future::ready(Json(info.clone()))))
}

/// The port Cloud Run assigns (`PORT`), 8080 by default.
pub fn port() -> Result<u16, ConfigError> {
    config::parse_or("PORT", 8080)
}

/// Resolves on SIGTERM (Cloud Run gives 10 s before SIGKILL) or Ctrl-C.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received; draining requests");
}

/// Serves `router` on `0.0.0.0:<port>` with the request context, until a shutdown signal. Handlers
/// may read the peer address (`axum::extract::ConnectInfo<SocketAddr>`): the client network of a
/// local stack, where no front end appends `X-Forwarded-For`. `FI_CLIENT_ADDRESS` is read here
/// ([`ClientAddress`]; an invalid value fails before the port is bound) and handed to every request.
pub async fn serve(router: Router, port: u16) -> std::io::Result<()> {
    serve_on(router, std::net::SocketAddr::from(([0, 0, 0, 0], port))).await
}

/// [`serve`] on any address (a second, private listener: `fi-platform`'s `FI_INTERNAL_BIND`).
pub async fn serve_on(router: Router, addr: std::net::SocketAddr) -> std::io::Result<()> {
    let client = ClientAddress::from_env()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e.to_string()))?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let port = addr.port();
    tracing::info!(port, client_address = client.mode(), "listening");
    let app = router
        .layer(axum::Extension(std::sync::Arc::new(client)))
        .layer(axum::middleware::from_fn(request_context));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    #[test]
    fn parses_w3c_traceparent() {
        let t =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(t.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(t.span_id, "00f067aa0ba902b7");
        assert!(t.sampled);
        assert_eq!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01"),
            None
        );
        assert_eq!(
            parse_traceparent("01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
            None
        );
        assert_eq!(parse_traceparent("garbage"), None);
    }

    #[test]
    fn parses_cloud_trace_context() {
        let t = parse_cloud_trace("105445aa7843bc8bf206b12000100000/1;o=1").unwrap();
        assert_eq!(t.trace_id, "105445aa7843bc8bf206b12000100000");
        assert_eq!(t.span_id, "0000000000000001");
        assert!(t.sampled);
        assert!(
            !parse_cloud_trace("105445aa7843bc8bf206b12000100000/77")
                .unwrap()
                .sampled
        );
        assert_eq!(parse_cloud_trace("short/1;o=1"), None);
    }

    #[test]
    fn logged_paths_never_carry_a_xuid() {
        assert_eq!(
            log_path("/userpresence.xboxlive.com/users/xuid(2535100000000043)/groups/People"),
            "/userpresence.xboxlive.com/users/xuid(…)/groups/People"
        );
        assert_eq!(
            log_path("/launcher/v1/profiles/2535100000000046/save"),
            "/launcher/v1/profiles/…/save"
        );
        assert_eq!(log_path("/a/1234567890123"), "/a/…");
        assert_eq!(
            log_path("/gameserverds.xboxlive.com/titles/1368890819/variants"),
            "/gameserverds.xboxlive.com/titles/1368890819/variants"
        );
        assert_eq!(
            log_path("/x/123456789012/01a1532a-c977-73ba-888c-eb0a934ded1e"),
            "/x/123456789012/01a1532a-c977-73ba-888c-eb0a934ded1e"
        );
        assert_eq!(log_path(""), "");
        assert_eq!(log_path("/"), "/");
    }

    #[test]
    fn expected_refusals_are_quiet() {
        assert!(quiet_refusal(401) && quiet_refusal(404));
        assert!(!quiet_refusal(403) && !quiet_refusal(409) && !quiet_refusal(412));
    }

    fn client_address(vars: &[(&str, &str)]) -> Result<ClientAddress, ConfigError> {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ClientAddress::parse(&|name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    }

    #[test]
    fn the_client_address_mode_defaults_by_environment_and_refuses_bad_values() {
        assert_eq!(client_address(&[]).unwrap(), ClientAddress::Forwarded);
        assert_eq!(
            client_address(&[("FI_ENV", "local"), ("FI_CLIENT_ADDRESS", " ")]).unwrap(),
            ClientAddress::Forwarded,
            "blank is unset"
        );
        for prod in [("FI_ENV", "prod"), ("APP_ENV", "PROD")] {
            assert_eq!(
                client_address(&[prod]).unwrap(),
                ClientAddress::Peer,
                "prod's default is never forwarded"
            );
        }
        assert_eq!(
            client_address(&[("FI_ENV", "prod"), ("FI_CLIENT_ADDRESS", "forwarded")]).unwrap(),
            ClientAddress::Forwarded
        );
        assert_eq!(
            client_address(&[("FI_CLIENT_ADDRESS", "peer")]).unwrap(),
            ClientAddress::Peer
        );
        assert_eq!(
            client_address(&[
                ("FI_CLIENT_ADDRESS", "trusted-proxy"),
                ("FI_TRUSTED_PROXIES", "172.30.122.10/32, fd00::/8"),
            ])
            .unwrap()
            .mode(),
            "trusted-proxy"
        );
        let invalid = |r: Result<ClientAddress, ConfigError>| match r {
            Err(ConfigError::Invalid { name, .. }) | Err(ConfigError::Missing(name)) => name,
            Ok(c) => panic!("accepted {c:?}"),
        };
        assert_eq!(
            invalid(client_address(&[("FI_CLIENT_ADDRESS", "cloudflare")])),
            "FI_CLIENT_ADDRESS"
        );
        assert_eq!(
            invalid(client_address(&[("FI_CLIENT_ADDRESS", "trusted-proxy")])),
            "FI_TRUSTED_PROXIES",
            "trusted-proxy needs a network"
        );
        for bad in ["10.0.0.0/33", "10.0.0/8", "fd00::/129", "10.0.0.1/x", "any"] {
            assert_eq!(
                invalid(client_address(&[
                    ("FI_CLIENT_ADDRESS", "peer"),
                    ("FI_TRUSTED_PROXIES", bad)
                ])),
                "FI_TRUSTED_PROXIES",
                "{bad}"
            );
        }
    }

    #[test]
    fn networks_contain_their_addresses_only() {
        let net = Cidr::parse("172.30.122.0/24").unwrap();
        assert!(net.contains("172.30.122.10".parse().unwrap()));
        assert!(net.contains("::ffff:172.30.122.10".parse().unwrap()));
        assert!(!net.contains("172.30.123.10".parse().unwrap()));
        let host = Cidr::parse("172.30.122.10").unwrap();
        assert!(host.contains("172.30.122.10".parse().unwrap()));
        assert!(!host.contains("172.30.122.1".parse().unwrap()));
        let all = Cidr::parse("0.0.0.0/0").unwrap();
        assert!(all.contains("203.0.113.9".parse().unwrap()));
        assert!(!all.contains("2001:db8::1".parse().unwrap()));
        let v6 = Cidr::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:ffff::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));
        let proxies = ClientAddress::TrustedProxy(vec![host]);
        assert!(proxies.trusts("172.30.122.10".parse().unwrap()));
        assert!(!proxies.trusts("172.30.122.1".parse().unwrap()));
        assert!(!ClientAddress::Peer.trusts("172.30.122.10".parse().unwrap()));
    }

    #[test]
    fn edge_secrets_parse_and_match_in_rotation() {
        let one = "a".repeat(32);
        let two = "b".repeat(40);
        let s = EdgeSecret::parse(&format!("{one}, {two}")).unwrap();
        assert!(s.matches(one.as_bytes()) && s.matches(two.as_bytes()));
        assert!(!s.matches(b"") && !s.matches(&one.as_bytes()[..31]));
        assert!(!format!("{s:?}").contains(&one));
        assert!(EdgeSecret::parse("short").is_err());
        assert!(EdgeSecret::parse(&format!("{one},{one},{one}")).is_err());
        assert!(EdgeSecret::parse(" , ").is_err());
        assert!(EdgeSecret::parse(&format!("{} x", "c".repeat(32))).is_err());
        assert!(
            is_health("/health/ready") && !is_health("/healthz") && !is_health("/core/v1/health/x")
        );
    }

    #[tokio::test]
    async fn the_edge_lock_refuses_everything_but_health_without_the_header() {
        let secret = "s".repeat(32);
        let lock = Some(std::sync::Arc::new(EdgeSecret::parse(&secret).unwrap()));
        let app = health_router(|| async { Ok(()) })
            .route(
                "/core/v1/status",
                get(|req: Request| async move {
                    let seen = req.headers().contains_key(&EDGE_HEADER);
                    let verified = req.extensions().get::<EdgeVerified>().is_some();
                    Json(json!({"headerSeen": seen, "verified": verified}))
                }),
            )
            .layer(axum::middleware::from_fn_with_state(lock, edge_guard));
        let call = |uri: &str, edge: Option<&str>| {
            let mut b = Request::builder().uri(uri);
            if let Some(e) = edge {
                b = b.header(&EDGE_HEADER, e);
            }
            b.body(Body::empty()).unwrap()
        };
        let r = app
            .clone()
            .oneshot(call("/core/v1/status", None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = app
            .clone()
            .oneshot(call("/core/v1/status", Some("wrong")))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = app
            .clone()
            .oneshot(call("/core/v1/status", Some(&secret)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"headerSeen": false, "verified": true})
        );
        for health in ["/health/live", "/health/ready"] {
            let r = app.clone().oneshot(call(health, None)).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK, "{health}");
        }
        // Without a secret nothing is locked and nothing is marked verified.
        let open = Router::new()
            .route(
                "/x",
                get(|req: Request| async move {
                    Json(json!({"verified": req.extensions().get::<EdgeVerified>().is_some()}))
                }),
            )
            .layer(axum::middleware::from_fn_with_state(None, edge_guard));
        let r = open.oneshot(call("/x", Some(&secret))).await.unwrap();
        let body = axum::body::to_bytes(r.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"verified": false})
        );
    }

    #[tokio::test]
    async fn readiness_reports_its_dependency() {
        let app = health_router(|| async { Err::<(), _>("database unreachable".to_owned()) })
            .layer(axum::middleware::from_fn(request_context));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .header("x-request-id", "abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[&REQUEST_ID], "abc");
        let live = app
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);
        assert_eq!(
            live.headers()[&REQUEST_ID].len(),
            36,
            "a request id is generated when none came"
        );
    }
}
