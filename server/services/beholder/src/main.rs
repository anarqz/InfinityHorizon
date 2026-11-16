//! fi-beholder: the announcement directory (docs/contracts/ANNOUNCER_API.md, CR-S54). Servers and
//! relays announce themselves with an Ed25519-signed body; launchers read the lists. One process,
//! one SQLite file in `DATA_DIR`, no outbound request, no player data.
//!
//! | Variable | Default | What |
//! | --- | --- | --- |
//! | `PORT` | `8080` | listen port (all interfaces) |
//! | `DATA_DIR` | `.` | directory of `beholder.sqlite` |
//! | `FI_BEHOLDER_CLIENT_ADDRESS` | `peer` | `cf`: believe `CF-Connecting-IP` (only behind the tunnel) |
//!
//! Logs carry counts and kinds, never an endpoint, key or client address.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::{Signature, VerifyingKey};
use fi_protocol::beholder::{
    AnnounceRequest, KIND_RELAY, KIND_SERVER, Listing, ListingPage, MAX_ANNOUNCE_BYTES,
    MAX_LISTING_ITEMS,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

const MAX_BODY: usize = MAX_ANNOUNCE_BYTES;
const WINDOW_SECS: i64 = 120;
const LISTED_SECS: i64 = 90;
const FORGET_SECS: i64 = 7 * 24 * 3600;
const PER_KEY_PER_MIN: u32 = 12;
const PER_ADDR_PER_MIN: u32 = 60;
const MAX_ITEMS: i64 = MAX_LISTING_ITEMS as i64;

#[derive(Clone)]
struct App {
    db: Arc<Mutex<Connection>>,
    trust_cf: bool,
}

/// Counts one hit for `key` in the current minute (stored in SQLite, like every other state); false
/// past `max`.
fn hit(c: &Connection, key: &str, max: u32, now: i64) -> rusqlite::Result<bool> {
    let minute = now.div_euclid(60);
    c.execute("DELETE FROM hit WHERE minute < ?1", params![minute])?;
    let n: u32 = c.query_row(
        "INSERT INTO hit (key, minute, n) VALUES (?1, ?2, 1)
         ON CONFLICT (key, minute) DO UPDATE SET n = n + 1 RETURNING n",
        params![key, minute],
        |r| r.get(0),
    )?;
    Ok(n <= max)
}

fn error(status: StatusCode, code: &str, field: Option<&str>) -> Response {
    let mut body = serde_json::json!({ "error": { "code": code } });
    if let Some(f) = field {
        body["error"]["details"] = serde_json::json!({ "field": f });
    }
    (status, Json(body)).into_response()
}

fn now_unix() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn rfc3339(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// An ASCII DNS name or an IPv4 literal.
fn valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    if h.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    h.split('.').all(|l| {
        !l.is_empty()
            && l.len() <= 63
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    }) && h.bytes().any(|b| b.is_ascii_alphabetic())
}

fn valid_port(p: &str) -> bool {
    p.parse::<u16>().is_ok_and(|n| n > 0) && !p.starts_with('0')
}

/// The endpoint's host when it follows its kind's rule.
fn endpoint_host<'a>(kind: &str, e: &'a str) -> Option<&'a str> {
    let hostport = match kind {
        KIND_SERVER => {
            let rest = e
                .strip_prefix("https://")
                .or_else(|| e.strip_prefix("http://"))?;
            let rest = rest.strip_suffix('/').unwrap_or(rest);
            if rest.contains(['/', '?', '#', '@']) {
                return None;
            }
            rest
        }
        KIND_RELAY => e,
        _ => return None,
    };
    match hostport.rsplit_once(':') {
        Some((h, p)) if valid_host(h) && valid_port(p) => Some(h),
        None if kind == KIND_SERVER && valid_host(hostport) => Some(hostport),
        _ => None,
    }
}

fn charset(s: &str, extra: &[u8]) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// The field that breaks its rule, or the verified key.
fn validate(a: &AnnounceRequest, now: i64) -> Result<String, Box<Response>> {
    let bad = |f| {
        Err(Box::new(error(
            StatusCode::BAD_REQUEST,
            "REQUEST.INVALID",
            Some(f),
        )))
    };
    if a.kind != KIND_SERVER && a.kind != KIND_RELAY {
        return bad("kind");
    }
    let Ok(key_bytes) = B64.decode(&a.public_key) else {
        return bad("publicKey");
    };
    let Ok(key_arr) = <[u8; 32]>::try_from(key_bytes.as_slice()) else {
        return bad("publicKey");
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_arr) else {
        return bad("publicKey");
    };
    let Some(host) = endpoint_host(&a.kind, &a.endpoint) else {
        return bad("endpoint");
    };
    let n = a.name.chars().count();
    if !(1..=48).contains(&n) || a.name.chars().any(char::is_control) {
        return bad("name");
    }
    if a.region.len() > 16 || !charset(&a.region, b"-") {
        return bad("region");
    }
    if a.version.is_empty() || a.version.len() > 32 || !charset(&a.version, b".+-") {
        return bad("version");
    }
    if !(1..=1000).contains(&a.capacity) {
        return bad("capacity");
    }
    match (a.kind.as_str(), a.rendezvous_port) {
        (KIND_SERVER, Some(_)) => return bad("rendezvousPort"),
        (_, Some(p)) if !(1..=65535).contains(&p) => return bad("rendezvousPort"),
        _ => {}
    }
    let Ok(sig_bytes) = B64.decode(&a.signature) else {
        return bad("signature");
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return bad("signature");
    };
    if (now - a.issued_at).abs() > WINDOW_SECS {
        return Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "BEHOLDER.STALE",
            None,
        )));
    }
    if key
        .verify_strict(a.signed_message().as_bytes(), &sig)
        .is_err()
    {
        return Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "BEHOLDER.SIGNATURE_INVALID",
            None,
        )));
    }
    Ok(host.to_ascii_lowercase())
}

fn open(path: &std::path::Path) -> rusqlite::Result<Connection> {
    let c = Connection::open(path)?;
    migrate(&c)?;
    Ok(c)
}

/// Creates the tables, then adds the columns a file from an older image lacks
/// (`rendezvous_port`, CR-S57 follow-up: old rows keep NULL and list it absent).
fn migrate(c: &Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA busy_timeout = 5000;
         CREATE TABLE IF NOT EXISTS node (
           public_key TEXT PRIMARY KEY,
           kind       TEXT NOT NULL CHECK (kind IN ('server', 'relay')),
           endpoint   TEXT NOT NULL,
           host       TEXT NOT NULL,
           name       TEXT NOT NULL,
           region     TEXT NOT NULL,
           version    TEXT NOT NULL,
           capacity   INTEGER NOT NULL,
           first_seen INTEGER NOT NULL,
           last_seen  INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS node_kind_seen ON node (kind, last_seen DESC);
         CREATE TABLE IF NOT EXISTS blocked (value TEXT PRIMARY KEY, reason TEXT);
         CREATE TABLE IF NOT EXISTS hit (
           key TEXT NOT NULL, minute INTEGER NOT NULL, n INTEGER NOT NULL,
           PRIMARY KEY (key, minute)
         );",
    )?;
    let has_port: bool = c.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('node') WHERE name = 'rendezvous_port'",
        [],
        |r| r.get(0),
    )?;
    if !has_port {
        c.execute_batch("ALTER TABLE node ADD COLUMN rendezvous_port INTEGER")?;
    }
    Ok(())
}

fn client(headers: &HeaderMap, peer: SocketAddr, trust_cf: bool) -> IpAddr {
    if trust_cf
        && let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

async fn announce(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if body.len() > MAX_BODY {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "REQUEST.TOO_LARGE", None);
    }
    let addr = format!("a:{}", client(&headers, peer, app.trust_cf));
    let parsed = serde_json::from_slice::<AnnounceRequest>(&body);
    let db = app.db.clone();
    let outcome =
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Response> {
            let c = db.lock().expect("db");
            let now = now_unix();
            if !hit(&c, &addr, PER_ADDR_PER_MIN, now)? {
                return Ok(limited());
            }
            let Ok(a) = parsed else {
                return Ok(error(
                    StatusCode::BAD_REQUEST,
                    "REQUEST.INVALID",
                    Some("body"),
                ));
            };
            let host = match validate(&a, now) {
                Ok(h) => h,
                Err(r) => return Ok(*r),
            };
            if !hit(&c, &format!("k:{}", a.public_key), PER_KEY_PER_MIN, now)? {
                return Ok(limited());
            }
            let blocked: Option<String> = c
                .query_row(
                    "SELECT value FROM blocked WHERE value IN (?1, ?2)",
                    params![a.public_key, host],
                    |r| r.get(0),
                )
                .optional()?;
            if blocked.is_some() {
                c.execute(
                    "DELETE FROM node WHERE public_key = ?1",
                    params![a.public_key],
                )?;
                return Ok(error(StatusCode::FORBIDDEN, "BEHOLDER.BLOCKED", None));
            }
            c.execute(
            "INSERT INTO node (public_key, kind, endpoint, host, name, region, version, capacity,
                               first_seen, last_seen, rendezvous_port)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)
             ON CONFLICT (public_key) DO UPDATE SET kind = excluded.kind,
               endpoint = excluded.endpoint, host = excluded.host, name = excluded.name,
               region = excluded.region, version = excluded.version,
               capacity = excluded.capacity, last_seen = excluded.last_seen,
               rendezvous_port = excluded.rendezvous_port,
               first_seen = CASE WHEN node.kind = excluded.kind THEN node.first_seen
                                 ELSE excluded.first_seen END",
            params![
                a.public_key, a.kind, a.endpoint, host, a.name, a.region, a.version, a.capacity,
                now, a.rendezvous_port
            ],
        )?;
            eprintln!("{{\"message\":\"announce\",\"kind\":\"{}\"}}", a.kind);
            Ok(StatusCode::NO_CONTENT.into_response())
        })
        .await;
    match outcome {
        Ok(Ok(r)) => r,
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "INTERNAL.UNAVAILABLE",
            None,
        ),
    }
}

fn limited() -> Response {
    let mut r = error(StatusCode::TOO_MANY_REQUESTS, "REQUEST.RATE_LIMITED", None);
    r.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
    r
}

fn listings(c: &Connection, kind: &str, now: i64) -> rusqlite::Result<Vec<Listing>> {
    let mut s = c.prepare(
        "SELECT public_key, name, endpoint, region, version, capacity, first_seen, last_seen,
                rendezvous_port
         FROM node
         WHERE kind = ?1 AND last_seen >= ?2
           AND public_key NOT IN (SELECT value FROM blocked)
           AND host NOT IN (SELECT value FROM blocked)
         ORDER BY last_seen DESC LIMIT ?3",
    )?;
    let rows = s.query_map(params![kind, now - LISTED_SECS, MAX_ITEMS], |r| {
        Ok(Listing {
            public_key: r.get(0)?,
            name: r.get(1)?,
            endpoint: r.get(2)?,
            region: r.get(3)?,
            version: r.get(4)?,
            capacity: r.get(5)?,
            first_seen: rfc3339(r.get(6)?),
            last_seen: rfc3339(r.get(7)?),
            rendezvous_port: r.get(8)?,
        })
    })?;
    rows.collect()
}

async fn list(app: App, kind: &'static str) -> Response {
    let db = app.db.clone();
    let items =
        tokio::task::spawn_blocking(move || listings(&db.lock().expect("db"), kind, now_unix()))
            .await;
    match items {
        Ok(Ok(items)) => {
            let mut r = Json(ListingPage { items }).into_response();
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=15"),
            );
            r
        }
        _ => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "INTERNAL.UNAVAILABLE",
            None,
        ),
    }
}

fn router(app: App) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/beholder/v1/announce", post(announce))
        .route(
            "/beholder/v1/servers",
            get(|State(a): State<App>| list(a, KIND_SERVER)),
        )
        .route(
            "/beholder/v1/relays",
            get(|State(a): State<App>| list(a, KIND_RELAY)),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY + 1))
        .with_state(app)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::var("PORT").ok().map_or(Ok(8080), |p| p.parse())?;
    let dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| ".".into()));
    let trust_cf = match std::env::var("FI_BEHOLDER_CLIENT_ADDRESS").as_deref() {
        Err(_) | Ok("") | Ok("peer") => false,
        Ok("cf") => true,
        Ok(_) => return Err("FI_BEHOLDER_CLIENT_ADDRESS: peer or cf".into()),
    };
    std::fs::create_dir_all(&dir)?;
    let db = Arc::new(Mutex::new(open(&dir.join("beholder.sqlite"))?));
    let app = App {
        db: db.clone(),
        trust_cf,
    };
    // Forget nodes unseen for a week, hourly.
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let db = db.clone();
            let _ = tokio::task::spawn_blocking(move || {
                db.lock().expect("db").execute(
                    "DELETE FROM node WHERE last_seen < ?1",
                    params![now_unix() - FORGET_SECS],
                )
            })
            .await;
        }
    });
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    eprintln!(
        "{{\"message\":\"listening\",\"port\":{port},\"client_address\":\"{}\"}}",
        if trust_cf { "cf" } else { "peer" }
    );
    axum::serve(
        listener,
        router(app).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use ed25519_dalek::{Signer, SigningKey};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// The listing bytes of a fixed row: pinned so the move to `fi_protocol::beholder` changed
    /// nothing on the wire (field order, names, instant shape).
    #[test]
    fn the_listing_bytes_are_pinned() {
        let page = ListingPage {
            items: vec![Listing {
                public_key: "6kv0ZfhpGqTDOpHbHOWm6gF5c9YgM5cOJ9Szw0pVqjU=".into(),
                name: "Example Relay 1 (203.0.113.10)".into(),
                endpoint: "203.0.113.10:3478".into(),
                region: "test-east".into(),
                version: "0.0.0".into(),
                capacity: 30,
                first_seen: rfc3339(1_792_245_600),
                last_seen: rfc3339(1_793_000_000),
                rendezvous_port: None,
            }],
        };
        assert_eq!(
            serde_json::to_string(&page).unwrap(),
            r#"{"items":[{"publicKey":"6kv0ZfhpGqTDOpHbHOWm6gF5c9YgM5cOJ9Szw0pVqjU=","name":"Example Relay 1 (203.0.113.10)","endpoint":"203.0.113.10:3478","region":"test-east","version":"0.0.0","capacity":30,"firstSeen":"2026-10-17T14:00:00Z","lastSeen":"2026-10-26T07:33:20Z"}]}"#
        );
        let mut page = page;
        page.items[0].rendezvous_port = Some(5101);
        assert_eq!(
            serde_json::to_string(&page).unwrap(),
            r#"{"items":[{"publicKey":"6kv0ZfhpGqTDOpHbHOWm6gF5c9YgM5cOJ9Szw0pVqjU=","name":"Example Relay 1 (203.0.113.10)","endpoint":"203.0.113.10:3478","region":"test-east","version":"0.0.0","capacity":30,"firstSeen":"2026-10-17T14:00:00Z","lastSeen":"2026-10-26T07:33:20Z","rendezvousPort":5101}]}"#
        );
    }

    fn app() -> App {
        App {
            db: Arc::new(Mutex::new(open(std::path::Path::new(":memory:")).unwrap())),
            trust_cf: false,
        }
    }

    fn signed(sk: &SigningKey, kind: &str, endpoint: &str, at: i64) -> serde_json::Value {
        signed_port(sk, kind, endpoint, at, None)
    }

    fn signed_port(
        sk: &SigningKey,
        kind: &str,
        endpoint: &str,
        at: i64,
        port: Option<u32>,
    ) -> serde_json::Value {
        let mut a = AnnounceRequest {
            kind: kind.into(),
            public_key: B64.encode(sk.verifying_key().to_bytes()),
            endpoint: endpoint.into(),
            name: "SAURON".into(),
            region: "test-east".into(),
            version: "0.1.0".into(),
            capacity: 30,
            issued_at: at,
            signature: String::new(),
            rendezvous_port: port,
        };
        a.signature = B64.encode(sk.sign(a.signed_message().as_bytes()).to_bytes());
        let mut body = serde_json::json!({
            "kind": a.kind, "publicKey": a.public_key, "endpoint": a.endpoint, "name": a.name,
            "region": a.region, "version": a.version, "capacity": a.capacity,
            "issuedAt": a.issued_at, "signature": a.signature,
        });
        if let Some(p) = port {
            body["rendezvousPort"] = p.into();
        }
        body
    }

    async fn call(
        app: &App,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        req = req.header("content-type", "application/json");
        let mut req = req
            .body(body.map_or(Body::empty(), |b| Body::from(b.to_string())))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 7], 5000))));
        let res = router(app.clone()).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    #[tokio::test]
    async fn a_signed_announce_is_listed_by_kind() {
        let a = app();
        let (s, _) = call(
            &a,
            "POST",
            "/beholder/v1/announce",
            Some(signed(
                &key(1),
                "server",
                "https://play.example.com",
                now_unix(),
            )),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _) = call(
            &a,
            "POST",
            "/beholder/v1/announce",
            Some(signed(&key(2), "relay", "203.0.113.10:3478", now_unix())),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, servers) = call(&a, "GET", "/beholder/v1/servers", None).await;
        assert_eq!(servers["items"].as_array().unwrap().len(), 1);
        assert_eq!(servers["items"][0]["endpoint"], "https://play.example.com");
        let (_, relays) = call(&a, "GET", "/beholder/v1/relays", None).await;
        assert_eq!(relays["items"][0]["capacity"], 30);
    }

    #[tokio::test]
    async fn a_forged_or_stale_announce_is_refused() {
        let a = app();
        let mut body = signed(&key(1), "server", "https://play.example.com", now_unix());
        body["endpoint"] = "https://evil.example.com".into();
        let (s, e) = call(&a, "POST", "/beholder/v1/announce", Some(body)).await;
        assert_eq!(
            (s, e["error"]["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("BEHOLDER.SIGNATURE_INVALID"))
        );
        let (s, e) = call(
            &a,
            "POST",
            "/beholder/v1/announce",
            Some(signed(
                &key(1),
                "server",
                "https://play.example.com",
                now_unix() - 600,
            )),
        )
        .await;
        assert_eq!(
            (s, e["error"]["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("BEHOLDER.STALE"))
        );
        let (_, servers) = call(&a, "GET", "/beholder/v1/servers", None).await;
        assert!(servers["items"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_endpoints_are_refused() {
        let a = app();
        for (kind, e) in [
            ("server", "https://play.example.com/path"),
            ("server", "https://user@play.example.com"),
            ("server", "ftp://play.example.com"),
            ("server", "https://[::1]:8080"),
            ("server", "https://play.example.com:0"),
            ("relay", "203.0.113.10"),
            ("relay", "http://203.0.113.10:3478"),
            ("relay", "203.0.113.10:99999"),
        ] {
            let (s, err) = call(
                &a,
                "POST",
                "/beholder/v1/announce",
                Some(signed(&key(3), kind, e, now_unix())),
            )
            .await;
            assert_eq!(
                (s, err["error"]["details"]["field"].as_str()),
                (StatusCode::BAD_REQUEST, Some("endpoint")),
                "{e}"
            );
        }
    }

    #[tokio::test]
    async fn a_blocked_key_or_host_is_refused_and_unlisted() {
        let a = app();
        let body = signed(&key(4), "server", "https://bad.example.com", now_unix());
        assert_eq!(
            call(&a, "POST", "/beholder/v1/announce", Some(body))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        a.db.lock()
            .unwrap()
            .execute(
                "INSERT INTO blocked (value, reason) VALUES ('bad.example.com', 't')",
                [],
            )
            .unwrap();
        let (_, servers) = call(&a, "GET", "/beholder/v1/servers", None).await;
        assert!(servers["items"].as_array().unwrap().is_empty());
        let (s, e) = call(
            &a,
            "POST",
            "/beholder/v1/announce",
            Some(signed(
                &key(4),
                "server",
                "https://bad.example.com",
                now_unix(),
            )),
        )
        .await;
        assert_eq!(
            (s, e["error"]["code"].as_str()),
            (StatusCode::FORBIDDEN, Some("BEHOLDER.BLOCKED"))
        );
    }

    #[tokio::test]
    async fn one_key_is_rate_limited() {
        let a = app();
        for i in 0..PER_KEY_PER_MIN {
            let (s, _) = call(
                &a,
                "POST",
                "/beholder/v1/announce",
                Some(signed(
                    &key(5),
                    "relay",
                    "relay.example.com:3478",
                    now_unix(),
                )),
            )
            .await;
            assert_eq!(s, StatusCode::NO_CONTENT, "{i}");
        }
        let (s, _) = call(
            &a,
            "POST",
            "/beholder/v1/announce",
            Some(signed(
                &key(5),
                "relay",
                "relay.example.com:3478",
                now_unix(),
            )),
        )
        .await;
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    }

    /// The nodes' announcer (crates/fi-announce) posting over real HTTP to this router: its body
    /// and signed message are what `validate` accepts, and the 13th post of a minute is a 429 the
    /// announcer reads as a 60 s wait.
    #[tokio::test]
    async fn the_node_announcer_is_accepted_over_http() {
        use fi_announce::{Announcer, Kind, Node, NodeKey, Outcome};
        let a = app();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = router(a.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, served).await });
        let base = format!("http://{addr}");
        let server = Node {
            kind: Kind::Server,
            endpoint: "https://play.example.test".into(),
            name: "Example Beta (play.example.test)".into(),
            region: String::new(),
            version: fi_announce::version_string("0.0.0", "fi-infinity-00007-x"),
            capacity: 50,
            rendezvous_port: None,
        };
        let relay = Node {
            kind: Kind::Relay,
            endpoint: "203.0.113.10:3478".into(),
            name: "Example Relay 1 (203.0.113.10)".into(),
            region: "test-east".into(),
            version: "0.0.0".into(),
            capacity: 30,
            rendezvous_port: Some(5101),
        };
        let s = Announcer::new(&base, NodeKey::generate().unwrap()).unwrap();
        let r = Announcer::new(&base, NodeKey::generate().unwrap()).unwrap();
        assert_eq!(s.announce_once(&server).await, Outcome::Listed);
        assert_eq!(r.announce_once(&relay).await, Outcome::Listed);
        let (_, servers) = call(&a, "GET", "/beholder/v1/servers", None).await;
        assert_eq!(servers["items"][0]["name"], server.name.as_str());
        assert_eq!(servers["items"][0]["publicKey"], s.public_key().as_str());
        assert_eq!(servers["items"][0]["version"], "0.0.0+fi-infinity-00007-x");
        assert!(servers["items"][0].get("rendezvousPort").is_none());
        let (_, relays) = call(&a, "GET", "/beholder/v1/relays", None).await;
        assert_eq!(relays["items"][0]["endpoint"], "203.0.113.10:3478");
        assert_eq!(relays["items"][0]["capacity"], 30);
        assert_eq!(relays["items"][0]["rendezvousPort"], 5101);
        // One key, one minute: 12 accepted (one above), then the limit.
        for _ in 1..PER_KEY_PER_MIN {
            r.announce_once(&relay).await;
        }
        assert_eq!(
            r.announce_once(&relay).await,
            Outcome::Limited(Duration::from_secs(60))
        );
        // A body the announcer signed for other fields is refused (the signature covers them).
        let key = NodeKey::generate().unwrap();
        let mut body = fi_announce::body(&key, &server, now_unix());
        body["capacity"] = 51.into();
        let (st, e) = call(&a, "POST", "/beholder/v1/announce", Some(body)).await;
        assert_eq!(
            (st, e["error"]["code"].as_str()),
            (StatusCode::UNAUTHORIZED, Some("BEHOLDER.SIGNATURE_INVALID"))
        );
        // So is a relay body whose rendezvous port was changed, added or removed after signing.
        let mut body = fi_announce::body(&key, &relay, now_unix());
        body["rendezvousPort"] = 5102.into();
        let mut added = fi_announce::body(
            &key,
            &Node {
                rendezvous_port: None,
                ..relay.clone()
            },
            now_unix(),
        );
        added["rendezvousPort"] = 5101.into();
        let mut removed = fi_announce::body(&key, &relay, now_unix());
        removed.as_object_mut().unwrap().remove("rendezvousPort");
        for body in [body, added, removed] {
            let (st, e) = call(&a, "POST", "/beholder/v1/announce", Some(body)).await;
            assert_eq!(
                (st, e["error"]["code"].as_str()),
                (StatusCode::UNAUTHORIZED, Some("BEHOLDER.SIGNATURE_INVALID"))
            );
        }
    }

    /// CR-S57 follow-up: `rendezvousPort` is a relay's, 1-65535; a relay announcing it lists it, an
    /// announce without it clears it.
    #[tokio::test]
    async fn a_relay_lists_its_rendezvous_port() {
        let a = app();
        for (kind, endpoint, port) in [
            ("server", "https://play.example.com", Some(5101)),
            ("relay", "relay.example.com:3478", Some(0)),
            ("relay", "relay.example.com:3478", Some(65536)),
        ] {
            let (s, e) = call(
                &a,
                "POST",
                "/beholder/v1/announce",
                Some(signed_port(&key(7), kind, endpoint, now_unix(), port)),
            )
            .await;
            assert_eq!(
                (
                    s,
                    e["error"]["code"].as_str(),
                    e["error"]["details"]["field"].as_str()
                ),
                (
                    StatusCode::BAD_REQUEST,
                    Some("REQUEST.INVALID"),
                    Some("rendezvousPort")
                ),
                "{kind} {port:?}"
            );
        }
        let mut body = signed(&key(7), "relay", "relay.example.com:3478", now_unix());
        body["rendezvousPort"] = (-1).into();
        let (s, e) = call(&a, "POST", "/beholder/v1/announce", Some(body)).await;
        assert_eq!(
            (s, e["error"]["details"]["field"].as_str()),
            (StatusCode::BAD_REQUEST, Some("body"))
        );
        let post = |port| {
            call(
                &a,
                "POST",
                "/beholder/v1/announce",
                Some(signed_port(
                    &key(7),
                    "relay",
                    "relay.example.com:3478",
                    now_unix(),
                    port,
                )),
            )
        };
        assert_eq!(post(Some(65535)).await.0, StatusCode::NO_CONTENT);
        let (_, relays) = call(&a, "GET", "/beholder/v1/relays", None).await;
        assert_eq!(relays["items"][0]["rendezvousPort"], 65535);
        assert_eq!(post(None).await.0, StatusCode::NO_CONTENT);
        let (_, relays) = call(&a, "GET", "/beholder/v1/relays", None).await;
        assert!(relays["items"][0].get("rendezvousPort").is_none());
    }

    /// A data file written by the image before the column: it gains the column, its rows list
    /// without `rendezvousPort`, and a new announce stores it.
    #[tokio::test]
    async fn an_older_data_file_gains_the_rendezvous_port_column() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE node (
               public_key TEXT PRIMARY KEY,
               kind       TEXT NOT NULL CHECK (kind IN ('server', 'relay')),
               endpoint   TEXT NOT NULL,
               host       TEXT NOT NULL,
               name       TEXT NOT NULL,
               region     TEXT NOT NULL,
               version    TEXT NOT NULL,
               capacity   INTEGER NOT NULL,
               first_seen INTEGER NOT NULL,
               last_seen  INTEGER NOT NULL
             );",
        )
        .unwrap();
        let now = now_unix();
        c.execute(
            "INSERT INTO node VALUES ('old', 'relay', 'old.example.com:3478', 'old.example.com',
               'old', '', '0.0.0', 5, ?1, ?1)",
            params![now - 10],
        )
        .unwrap();
        migrate(&c).unwrap();
        migrate(&c).unwrap();
        let a = App {
            db: Arc::new(Mutex::new(c)),
            trust_cf: false,
        };
        let (s, _) = call(
            &a,
            "POST",
            "/beholder/v1/announce",
            Some(signed_port(
                &key(8),
                "relay",
                "new.example.com:3478",
                now_unix(),
                Some(5101),
            )),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, relays) = call(&a, "GET", "/beholder/v1/relays", None).await;
        let items = relays["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["rendezvousPort"], 5101);
        assert_eq!(items[1]["publicKey"], "old");
        assert!(items[1].get("rendezvousPort").is_none());
    }

    #[tokio::test]
    async fn an_old_listing_drops_out() {
        let a = app();
        call(
            &a,
            "POST",
            "/beholder/v1/announce",
            Some(signed(
                &key(6),
                "server",
                "http://192.168.1.40:8080",
                now_unix(),
            )),
        )
        .await;
        a.db.lock()
            .unwrap()
            .execute("UPDATE node SET last_seen = last_seen - 120", [])
            .unwrap();
        let (_, servers) = call(&a, "GET", "/beholder/v1/servers", None).await;
        assert!(servers["items"].as_array().unwrap().is_empty());
    }
}
