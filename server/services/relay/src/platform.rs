//! The relay → platform calls (MATCHMAKING_API.md §5d "The rules", "Reports", "Failure behaviour"):
//! one task holding the per-start session, the report outbox ([`OUTBOX`], preallocated; a full one
//! drops its oldest) and the rules fetch.
//!
//! - **Rules:** `POST /mm/v1/relay/watch` at start, then every 300 s ± 30 s, and at once when a
//!   report answer names another version; a failure keeps the rules held and retries 5 s doubling
//!   to 300 s. Never a built-in default.
//! - **Reports:** a new report is sent at once (within 2 s); otherwise a counters-only batch every
//!   60 s. Up to 64 per batch, each `id` kept across retries (a new `seq`, `iat` and signature per
//!   attempt). A timeout (connect 2 s, call 5 s), refused connection, 5xx or an answer failing its
//!   signature retries 1 s doubling to 60 s; 429 waits its `Retry-After`; 401 retries every 60 s;
//!   400 drops the batch's reports. A report older than 10 minutes is dropped unsent.
//!
//! Logs carry counts and outcomes only, never an id, a key, an address or a body.

use ed25519_dalek::{SigningKey, VerifyingKey};
use fi_netwatch::Rules;
use fi_netwatch::client::{self, Session};
use fi_netwatch::detect::{Counters, Finding};
use fi_protocol::relay_watch::{MAX_REPORTS, RelayReport, SIGNATURE_HEADER};
use fi_protocol::routes::mm;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

/// Pending reports at most (the oldest is dropped past it).
pub const OUTBOX: usize = 1024;
/// A report whose `to` is older than this is dropped unsent (ms).
pub const MAX_AGE_MS: i64 = 10 * 60 * 1000;
/// Counters-only batch interval.
pub const IDLE_BATCH: Duration = Duration::from_secs(60);

/// What the reporter needs.
pub struct Platform {
    /// `FI_RELAY_PLATFORM_URL`, no trailing slash.
    pub base: String,
    /// The node key (`FI_ANNOUNCE_SIGNING_KEY` / `FI_ANNOUNCE_KEY_FILE`).
    pub node: SigningKey,
    /// `FI_RELAY_GRANT_VERIFY_KEYS`: matchmaking's pinned certificate issuers.
    pub issuers: Vec<(String, VerifyingKey)>,
    /// The HTTPS client.
    pub http: reqwest::Client,
}

impl std::fmt::Debug for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Platform").finish_non_exhaustive()
    }
}

/// `FI_RELAY_PLATFORM_URL`: `https://host[:port]` (`http://` only for a loopback host), no path.
pub fn platform_url(text: &str) -> Result<String, String> {
    let t = text.trim().trim_end_matches('/');
    let bad = || {
        "FI_RELAY_PLATFORM_URL: https://host[:port] (http:// for a loopback host only)".to_owned()
    };
    let (scheme, rest) = t.split_once("://").ok_or_else(bad)?;
    if rest.is_empty() || rest.contains(['/', '?', '#', '@']) {
        return Err(bad());
    }
    let host = rest.rsplit_once(':').map_or(rest, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            rest
        }
    });
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "[::1]");
    match scheme {
        "https" => Ok(t.to_owned()),
        "http" if loopback => Ok(t.to_owned()),
        _ => Err(bad()),
    }
}

/// `FI_RELAY_GRANT_VERIFY_KEYS` as certificate issuers (`kid:base64(public key)`).
pub fn issuers(text: &str) -> Result<Vec<(String, VerifyingKey)>, String> {
    let mut out = Vec::new();
    for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (kid, key) = item
            .split_once(':')
            .ok_or("FI_RELAY_GRANT_VERIFY_KEYS: kid:base64 key")?;
        let key = fi_netwatch::cert::key32(key.trim())
            .and_then(|k| VerifyingKey::from_bytes(&k).ok())
            .ok_or("FI_RELAY_GRANT_VERIFY_KEYS: a key is base64 of 32 bytes")?;
        out.push((kid.trim().to_owned(), key));
    }
    Ok(out)
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("the system random source is unavailable");
    b
}

fn unix_s() -> i64 {
    (crate::observe::now_ms() / 1000) as i64
}

enum Sent {
    Ok(fi_protocol::relay_watch::RelayReportAnswer),
    Retry,
    After(Duration),
    Unauthorized,
    BadRequest,
}

impl Platform {
    fn pinned(&self) -> impl Fn(&str) -> Option<VerifyingKey> + '_ {
        |kid| self.issuers.iter().find(|(k, _)| k == kid).map(|(_, v)| *v)
    }

    async fn post(&self, path: &str, body: Vec<u8>, sig: &str) -> Result<reqwest::Response, ()> {
        self.http
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .header(SIGNATURE_HEADER, sig)
            .body(body)
            .send()
            .await
            .map_err(|_| ())
    }

    async fn fetch_rules(&self, s: &Session) -> Option<Rules> {
        let nonce = client::uuid_text(&client::uuid4(random()));
        let (body, sig) = s.watch(unix_s(), &nonce);
        let r = self.post(mm::RELAY_WATCH, body, &sig).await.ok()?;
        if r.status() != reqwest::StatusCode::OK {
            tracing::info!(status = r.status().as_u16(), "relay rules fetch refused");
            return None;
        }
        let header = r.headers().get(SIGNATURE_HEADER)?.to_str().ok()?.to_owned();
        let bytes = r.bytes().await.ok()?;
        match client::verify_watch_answer(&bytes, &header, &nonce, self.pinned(), unix_s()) {
            Ok(w) => Some(client::rules(&w.rules)),
            Err(_) => {
                tracing::warn!("relay rules answer failed its signature");
                None
            }
        }
    }

    async fn send(
        &self,
        s: &Session,
        seq: i64,
        rules: Option<i64>,
        reports: Vec<RelayReport>,
        counters: fi_protocol::relay_watch::RelayCounters,
    ) -> Sent {
        let (body, sig) = s.batch(seq, unix_s(), rules, reports, counters);
        let Ok(r) = self.post(mm::RELAY_REPORTS, body, &sig).await else {
            return Sent::Retry;
        };
        let status = r.status();
        if status == reqwest::StatusCode::OK {
            let Some(header) = r
                .headers()
                .get(SIGNATURE_HEADER)
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned)
            else {
                return Sent::Retry;
            };
            let Ok(bytes) = r.bytes().await else {
                return Sent::Retry;
            };
            return match client::verify_report_answer(
                &bytes,
                &header,
                seq,
                &s.cert.start_id,
                self.pinned(),
                unix_s(),
            ) {
                Ok(a) => Sent::Ok(a),
                Err(_) => {
                    tracing::warn!("relay report answer failed its signature");
                    Sent::Retry
                }
            };
        }
        match status.as_u16() {
            401 => Sent::Unauthorized,
            400 | 413 => Sent::BadRequest,
            429 => {
                let secs = r
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(1)
                    .clamp(1, 300);
                Sent::After(Duration::from_secs(secs))
            }
            _ => Sent::Retry,
        }
    }
}

/// The reporter's channels.
pub struct Inputs {
    /// Findings from the detector.
    pub findings: mpsc::Receiver<Finding>,
    /// The detector's counters.
    pub counters: watch::Receiver<Counters>,
    /// Rules out to the detector.
    pub rules: watch::Sender<Option<Rules>>,
    /// Reports dropped (shared).
    pub dropped: Arc<AtomicU64>,
}

fn jitter(base: u64, spread: u64) -> Duration {
    let r = u64::from_le_bytes(random::<8>());
    Duration::from_secs(base - spread + r % (2 * spread + 1))
}

/// Runs the reporter until `stop`.
pub async fn run(p: Platform, mut inputs: Inputs, stop: impl Future<Output = ()>) {
    let start_id = client::uuid_text(&client::uuid4(random()));
    let mut session = Session::new(&p.node, random(), &start_id, unix_s());
    // Findings as the detector made them (fixed size): the wire form is built per attempt.
    let mut outbox: VecDeque<Finding> = VecDeque::with_capacity(OUTBOX);
    let mut seq: i64 = 0;
    let mut rules_version: Option<i64> = None;
    let mut next_rules = Instant::now();
    let mut rules_backoff = Duration::from_secs(5);
    let mut next_send = Instant::now() + IDLE_BATCH;
    let mut send_backoff = Duration::from_secs(1);
    let mut backing_off = false;
    let (mut sent, mut stored, mut refused) = (0u64, 0u64, 0u64);
    let mut log = tokio::time::interval(Duration::from_secs(300));
    tokio::pin!(stop);
    loop {
        let wake = next_send.min(next_rules);
        tokio::select! {
            () = &mut stop => return,
            _ = log.tick() => {
                tracing::info!(sent, stored, refused, pending = outbox.len(), "relay reports");
            }
            f = inputs.findings.recv() => {
                let Some(f) = f else { return };
                if outbox.len() >= OUTBOX {
                    outbox.pop_front();
                    inputs.dropped.fetch_add(1, Ordering::Relaxed);
                }
                outbox.push_back(f);
                if !backing_off {
                    next_send = next_send.min(Instant::now());
                }
            }
            () = tokio::time::sleep_until(wake) => {}
        }
        let now = Instant::now();
        if session.due(unix_s()) {
            // A new key and certificate; the start id (and the rings) continue.
            session = Session::new(&p.node, random(), &start_id, unix_s());
        }
        if now >= next_rules {
            match p.fetch_rules(&session).await {
                Some(r) => {
                    if rules_version != Some(r.version) {
                        tracing::info!(version = r.version, "relay rules");
                    }
                    rules_version = Some(r.version);
                    let _ = inputs.rules.send(Some(r));
                    rules_backoff = Duration::from_secs(5);
                    next_rules = Instant::now() + jitter(300, 30);
                }
                None => {
                    next_rules = Instant::now() + rules_backoff;
                    rules_backoff = (rules_backoff * 2).min(Duration::from_secs(300));
                }
            }
        }
        if now < next_send {
            continue;
        }
        let wall = crate::observe::now_ms() as i64;
        let before = outbox.len();
        outbox.retain(|f| wall - f.to_ms as i64 <= MAX_AGE_MS);
        inputs
            .dropped
            .fetch_add((before - outbox.len()) as u64, Ordering::Relaxed);
        let batch: Vec<RelayReport> = outbox
            .iter()
            .take(MAX_REPORTS)
            .map(client::report)
            .collect();
        let counters_now = *inputs.counters.borrow();
        let dropped_now = inputs.dropped.load(Ordering::Relaxed);
        // Totals since this start: a retry or a lost answer never counts twice (the platform keeps
        // the largest).
        let counters = client::counters(&counters_now, &Counters::default(), dropped_now);
        seq += 1;
        sent += 1;
        let n = batch.len();
        match p.send(&session, seq, rules_version, batch, counters).await {
            Sent::Ok(answer) => {
                stored += answer.accepted.len() as u64;
                outbox.retain(|f| !answer.accepted.contains(&client::uuid_text(&f.id)));
                backing_off = false;
                send_backoff = Duration::from_secs(1);
                next_send = if outbox.is_empty() {
                    Instant::now() + IDLE_BATCH
                } else {
                    Instant::now()
                };
                if rules_version != Some(answer.rules_version) {
                    next_rules = Instant::now();
                }
            }
            Sent::Retry => {
                backing_off = true;
                next_send = Instant::now() + send_backoff;
                send_backoff = (send_backoff * 2).min(Duration::from_secs(60));
            }
            Sent::After(wait) => {
                backing_off = true;
                next_send = Instant::now() + wait;
            }
            Sent::Unauthorized => {
                refused += 1;
                tracing::warn!(
                    "relay reports refused (401): not pinned, a bad certificate or a skewed clock"
                );
                backing_off = true;
                next_send = Instant::now() + Duration::from_secs(60);
            }
            Sent::BadRequest => {
                // A bug, never retried as is: the batch's reports go.
                tracing::warn!(reports = n, "relay report batch refused (400)");
                for _ in 0..n {
                    outbox.pop_front();
                }
                inputs.dropped.fetch_add(n as u64, Ordering::Relaxed);
                backing_off = false;
                next_send = if outbox.is_empty() {
                    Instant::now() + IDLE_BATCH
                } else {
                    Instant::now()
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_platform_url_is_https_or_loopback_http() {
        assert_eq!(
            platform_url("https://play.example.com/").unwrap(),
            "https://play.example.com"
        );
        assert!(platform_url("https://play.example.com:8443").is_ok());
        assert!(platform_url("http://127.0.0.1:8480").is_ok());
        assert!(platform_url("http://localhost:1").is_ok());
        for bad in [
            "http://play.example.com",
            "play.example.com",
            "https://play.example.com/mm",
            "https://u@play.example.com",
            "ftp://x",
            "https://",
        ] {
            assert!(platform_url(bad).is_err(), "{bad}");
        }
    }

    use fi_netwatch::cert;
    use fi_netwatch::detect::{FindingType, Kind};
    use fi_protocol::relay_watch::{
        MM_REPORT_ANSWER_PREFIX, MM_WATCH_ANSWER_PREFIX, RELAY_REPORT_PREFIX, RELAY_WATCH_PREFIX,
        RelayReportAnswer, RelayReportBatch, RelayWatch, RelayWatchRequest,
    };
    use std::io::{Read, Write};

    /// One HTTP/1.1 request: path, the signature header, the body.
    fn read_request(s: &mut std::net::TcpStream) -> (String, String, Vec<u8>) {
        let mut got = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "a request");
            got.extend_from_slice(&buf[..n]);
            if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&got[..end]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while got.len() < end + 4 + len {
                    let n = s.read(&mut buf).unwrap();
                    got.extend_from_slice(&buf[..n]);
                }
                let path = head.split(' ').nth(1).unwrap().to_owned();
                let sig = head
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("fi-relay-signature: ")
                            .or_else(|| l.strip_prefix("FI-Relay-Signature: "))
                    })
                    .unwrap_or_default()
                    .to_owned();
                return (path, sig, got[end + 4..end + 4 + len].to_vec());
            }
        }
    }

    fn respond(s: &mut std::net::TcpStream, body: &[u8], sig: &str) {
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nFI-Relay-Signature: {sig}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
    }

    /// A fake platform: verifies every request against the pinned node key and answers signed by
    /// a session certified by the relay-grant key `rg-1` (seed 8). Batches go to `seen`.
    fn fake_platform(node_pub: String, seen: std::sync::mpsc::Sender<RelayReportBatch>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let grant = SigningKey::from_bytes(&[8u8; 32]);
            let session = SigningKey::from_bytes(&[18u8; 32]);
            let now = unix_s();
            let mm = cert::mm_cert("rg-1", &grant, &session.verifying_key(), "inst", now);
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let (path, sig, body) = read_request(&mut s);
                if path == mm::RELAY_WATCH {
                    let req: RelayWatchRequest = serde_json::from_slice(&body).unwrap();
                    let key =
                        cert::verify_relay_cert(&req.cert, |k| k == node_pub, unix_s()).unwrap();
                    cert::verify_body(&key, RELAY_WATCH_PREFIX, &body, &sig).unwrap();
                    let answer = RelayWatch {
                        nonce: req.nonce,
                        rules: fi_protocol::matchmaking::WatchRules {
                            version: 1,
                            speed: fi_protocol::matchmaking::WatchSpeed {
                                max_kmh: 451.0,
                                sustain_ms: 3000,
                                per_car_floor_kmh: 311.3,
                            },
                            climb: fi_protocol::matchmaking::WatchLimit {
                                max_kmh: 150.0,
                                sustain_ms: 3000,
                            },
                            jump: fi_protocol::matchmaking::WatchJump { max_meters: 200.0 },
                        },
                        cert: mm.clone(),
                    };
                    let bytes = serde_json::to_vec(&answer).unwrap();
                    respond(
                        &mut s,
                        &bytes,
                        &cert::sign_body(&session, MM_WATCH_ANSWER_PREFIX, &bytes),
                    );
                } else {
                    let batch: RelayReportBatch = serde_json::from_slice(&body).unwrap();
                    let key =
                        cert::verify_relay_cert(&batch.cert, |k| k == node_pub, unix_s()).unwrap();
                    cert::verify_body(&key, RELAY_REPORT_PREFIX, &body, &sig).unwrap();
                    let answer = RelayReportAnswer {
                        seq: batch.seq,
                        start_id: batch.start_id.clone(),
                        accepted: batch.reports.iter().map(|r| r.id.clone()).collect(),
                        rules_version: 1,
                        cert: mm.clone(),
                    };
                    let _ = seen.send(batch);
                    let bytes = serde_json::to_vec(&answer).unwrap();
                    respond(
                        &mut s,
                        &bytes,
                        &cert::sign_body(&session, MM_REPORT_ANSWER_PREFIX, &bytes),
                    );
                }
            }
        });
        base
    }

    /// The reporter fetches the rules at start (signed, with a nonce the answer echoes), hands them
    /// to the detector, sends a new finding at once in a signed batch, and empties its outbox when
    /// the platform's signed answer accepts it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rules_arrive_and_a_finding_is_sent_signed_at_once() {
        let node = SigningKey::from_bytes(&[3u8; 32]);
        let node_pub = cert::b64(node.verifying_key().as_bytes());
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        let base = fake_platform(node_pub.clone(), seen_tx);
        let p = Platform {
            base,
            node,
            issuers: issuers(&format!(
                "rg-1:{}",
                cert::b64(
                    SigningKey::from_bytes(&[8u8; 32])
                        .verifying_key()
                        .as_bytes()
                )
            ))
            .unwrap(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        };
        let (ftx, frx) = mpsc::channel(8);
        let (_ctx, crx) = watch::channel(Counters::default());
        let (rtx, mut rrx) = watch::channel(None);
        let dropped = Arc::new(AtomicU64::new(0));
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(run(
            p,
            Inputs {
                findings: frx,
                counters: crx,
                rules: rtx,
                dropped: dropped.clone(),
            },
            async move {
                let _ = stop_rx.await;
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), rrx.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rrx.borrow().unwrap().speed.max_kmh, 451.0);
        let now_ms = crate::observe::now_ms();
        ftx.send(Finding {
            id: [1; 16],
            episode: [2; 16],
            kind_type: FindingType::Violation,
            kind: Kind::Speed,
            world: [3; 16],
            xuid: 11,
            host_xuid: 22,
            from_ms: now_ms - 5000,
            to_ms: now_ms - 2000,
            rules_version: 1,
            peak_kmh: 500.0,
            over_limit_ms: 3000,
            own_peak_kmh: None,
            legs: None,
            max_jump_m: None,
        })
        .await
        .unwrap();
        let batch =
            tokio::task::spawn_blocking(move || seen_rx.recv_timeout(Duration::from_secs(5)))
                .await
                .unwrap()
                .expect("a batch");
        assert_eq!(batch.reports.len(), 1);
        assert_eq!(batch.reports[0].id, "01010101-0101-0101-0101-010101010101");
        assert_eq!(batch.rules_version, Some(1));
        assert_eq!(batch.relay, node_pub);
        stop_tx.send(()).unwrap();
        task.await.unwrap();
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn issuers_parse_the_verify_keys() {
        let k = issuers("rg-local-1:A4Qtlcgz2EIO20E32DMyuO5ss4tu84VRWw51EeB04wg=").unwrap();
        assert_eq!(k[0].0, "rg-local-1");
        assert!(issuers("nokey").is_err());
        assert!(issuers("a:AAAA").is_err());
    }
}
