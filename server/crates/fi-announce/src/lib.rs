//! A node's side of the beholder (docs/contracts/ANNOUNCER_API.md, CR-S54): the node's Ed25519
//! identity ([`NodeKey`]), the announce body and its signed message (§2), and the loop that posts
//! it every 30 s (±3 s) until the process stops ([`Announcer::run`]).
//!
//! - The body is checked against the beholder's field rules before the first post
//!   ([`Node::validate`]), so a misconfigured node fails at start instead of being refused forever.
//! - A `429` waits its `Retry-After` (default 60 s, at most 300 s) instead of the cadence; any other
//!   answer or a network failure keeps the cadence: the loop never stops on its own.
//! - Logs carry outcome counts, an HTTP status or an error kind, never the beholder URL, the
//!   endpoint, the key or an address (a `reqwest::Error`'s text names the URL, so it is never logged).

use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::{Signer, SigningKey};
use fi_protocol::beholder::{AnnounceRequest, KIND_RELAY, KIND_SERVER};
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The cadence of ANNOUNCER_API §2.
pub const INTERVAL: Duration = Duration::from_secs(30);
/// The cadence's random spread, either way.
pub const JITTER: Duration = Duration::from_secs(3);
/// One post's whole budget.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// A `429` without a readable `Retry-After`.
const RETRY_AFTER_DEFAULT: u64 = 60;
/// The longest `Retry-After` honoured.
const RETRY_AFTER_MAX: u64 = 300;
/// A count line every this many posts even without a change (about an hour).
const SUMMARY_EVERY: u64 = 120;

/// What a node is (§2 `kind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An Infinity server: `endpoint` is `http(s)://host[:port]`.
    Server,
    /// A relay: `endpoint` is `host:port` (the UDP binding port).
    Relay,
}

impl Kind {
    /// The wire word.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Server => KIND_SERVER,
            Kind::Relay => KIND_RELAY,
        }
    }
}

/// A key that cannot be read, parsed or written. Never carries the value.
#[derive(Debug)]
pub enum KeyError {
    /// Not base64 of 32 bytes.
    Invalid,
    /// The OS random source failed.
    Random,
    /// The key file could not be read or written.
    Io(std::io::Error),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Invalid => f.write_str("the announce key is not base64 of 32 bytes"),
            KeyError::Random => f.write_str("the system random source is unavailable"),
            KeyError::Io(e) => write!(f, "the announce key file: {e}"),
        }
    }
}

impl std::error::Error for KeyError {}

/// The node's identity in the directory (§1): an Ed25519 key whose text form is base64 (standard,
/// padded) of its 32-byte seed. `Debug` never shows it.
#[derive(Clone)]
pub struct NodeKey(SigningKey);

impl std::fmt::Debug for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NodeKey(..)")
    }
}

impl NodeKey {
    /// A new key from the OS random source.
    pub fn generate() -> Result<Self, KeyError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|_| KeyError::Random)?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// Parses the text form (surrounding whitespace ignored).
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        let bytes = B64.decode(text.trim()).map_err(|_| KeyError::Invalid)?;
        let seed = <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| KeyError::Invalid)?;
        Ok(Self(SigningKey::from_bytes(&seed)))
    }

    /// The text form (a secret: write it only to a 0600 file).
    pub fn to_text(&self) -> String {
        B64.encode(self.0.to_bytes())
    }

    /// The public key as the beholder lists it (`publicKey`).
    pub fn public_key(&self) -> String {
        B64.encode(self.0.verifying_key().to_bytes())
    }

    /// The key in `path`, or a new one written there once (mode 0600, through a temporary file
    /// hard-linked into place, so an existing file is never replaced and never seen half-written).
    pub fn load_or_create(path: &Path) -> Result<Self, KeyError> {
        if path.exists() {
            return Self::parse(&std::fs::read_to_string(path).map_err(KeyError::Io)?);
        }
        let key = Self::generate()?;
        let mut tag = [0u8; 6];
        getrandom::fill(&mut tag).map_err(|_| KeyError::Random)?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let partial = path.with_file_name(format!(
            ".{name}.{}.partial",
            tag.iter().map(|b| format!("{b:02x}")).collect::<String>()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = (|| {
            let mut file = options.open(&partial)?;
            file.write_all(key.to_text().as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::hard_link(&partial, path)
        })();
        let _ = std::fs::remove_file(&partial);
        match written {
            Ok(()) => Ok(key),
            // Another start wrote it meanwhile: theirs is the identity.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::parse(&std::fs::read_to_string(path).map_err(KeyError::Io)?)
            }
            Err(e) => Err(KeyError::Io(e)),
        }
    }

    fn sign(&self, message: &str) -> String {
        B64.encode(self.0.sign(message.as_bytes()).to_bytes())
    }
}

/// What a node announces (everything of §2 but the key, the instant and the signature).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// Server or relay.
    pub kind: Kind,
    /// Where launchers reach it (the shape depends on `kind`).
    pub endpoint: String,
    /// 1-48 characters, no control character.
    pub name: String,
    /// 0-16 of `[A-Za-z0-9-]`.
    pub region: String,
    /// 1-32 of `[A-Za-z0-9.+-]` ([`version_string`]).
    pub version: String,
    /// 1-1000 ([`clamp_capacity`]).
    pub capacity: u32,
    /// A relay's UDP rendezvous port (`rendezvousPort`, 1-65535); always `None` on a server.
    pub rendezvous_port: Option<u16>,
}

/// An ASCII DNS name or an IPv4 literal (the beholder's rule).
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

/// The endpoint's host when it follows `kind`'s rule (§2).
pub fn endpoint_host(kind: Kind, endpoint: &str) -> Option<&str> {
    let hostport = match kind {
        Kind::Server => {
            let rest = endpoint
                .strip_prefix("https://")
                .or_else(|| endpoint.strip_prefix("http://"))?;
            let rest = rest.strip_suffix('/').unwrap_or(rest);
            if rest.contains(['/', '?', '#', '@']) {
                return None;
            }
            rest
        }
        Kind::Relay => endpoint,
    };
    match hostport.rsplit_once(':') {
        Some((h, p)) if valid_host(h) && valid_port(p) => Some(h),
        None if kind == Kind::Server && valid_host(hostport) => Some(hostport),
        _ => None,
    }
}

fn charset(s: &str, extra: &[u8]) -> bool {
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

impl Node {
    /// The first field (wire name) that breaks its rule (§2).
    pub fn validate(&self) -> Result<(), &'static str> {
        if endpoint_host(self.kind, &self.endpoint).is_none() {
            return Err("endpoint");
        }
        let n = self.name.chars().count();
        if !(1..=48).contains(&n) || self.name.chars().any(char::is_control) {
            return Err("name");
        }
        if self.region.len() > 16 || !charset(&self.region, b"-") {
            return Err("region");
        }
        if self.version.is_empty() || self.version.len() > 32 || !charset(&self.version, b".+-") {
            return Err("version");
        }
        if !(1..=1000).contains(&self.capacity) {
            return Err("capacity");
        }
        match (self.kind, self.rendezvous_port) {
            (Kind::Server, Some(_)) | (Kind::Relay, Some(0)) => return Err("rendezvousPort"),
            _ => {}
        }
        Ok(())
    }
}

/// The §2 request for `node` before its signature (`signature` empty).
fn unsigned(node: &Node, public_key: &str, issued_at: i64) -> AnnounceRequest {
    AnnounceRequest {
        kind: node.kind.as_str().to_owned(),
        public_key: public_key.to_owned(),
        endpoint: node.endpoint.clone(),
        name: node.name.clone(),
        region: node.region.clone(),
        version: node.version.clone(),
        capacity: node.capacity,
        issued_at,
        signature: String::new(),
        rendezvous_port: node.rendezvous_port.map(u32::from),
    }
}

/// The signed message of §2: the fields joined by `\n`, no trailing newline.
pub fn message(node: &Node, public_key: &str, issued_at: i64) -> String {
    unsigned(node, public_key, issued_at).signed_message()
}

/// The §2 request for `node`, signed by `key` at `issued_at` (Unix seconds).
pub fn request(key: &NodeKey, node: &Node, issued_at: i64) -> AnnounceRequest {
    let mut request = unsigned(node, &key.public_key(), issued_at);
    request.signature = key.sign(&request.signed_message());
    request
}

/// The announce body of §2, signed by `key` at `issued_at` (Unix seconds): exactly the nine fields,
/// plus `rendezvousPort` when the node has one (the beholder refuses unknown ones). A JSON value, so the posted keys keep their sorted order.
pub fn body(key: &NodeKey, node: &Node, issued_at: i64) -> serde_json::Value {
    serde_json::to_value(request(key, node, issued_at)).expect("a struct of strings and integers")
}

/// `version+revision` within the `version` rule: other characters become `-`, at most 32. A
/// revision `local` (no Cloud Run revision) is left out.
pub fn version_string(version: &str, revision: &str) -> String {
    let text = if revision.is_empty() || revision == "local" {
        version.to_owned()
    } else {
        format!("{version}+{revision}")
    };
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    if out.is_empty() {
        out.push('0');
    }
    out
}

/// Clamps a capacity into 1-1000 (`None`, unlimited, is 1000).
pub fn clamp_capacity(capacity: Option<u64>) -> u32 {
    capacity.map_or(1000, |c| c.clamp(1, 1000) as u32)
}

/// A post's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `204`: listed or refreshed.
    Listed,
    /// `429`: wait this long.
    Limited(Duration),
    /// Any other HTTP answer (a field, the signature, the clock, the block list).
    Refused(u16),
    /// No answer: `timeout`, `connect` or `other`.
    Failed(&'static str),
}

impl Outcome {
    fn label(&self) -> &'static str {
        match self {
            Outcome::Listed => "listed",
            Outcome::Limited(_) => "rate_limited",
            Outcome::Refused(_) => "refused",
            Outcome::Failed(_) => "failed",
        }
    }
}

/// The wait after `outcome`: the cadence moved by `jitter_ms` (within ±[`JITTER`] in production),
/// or a `429`'s `Retry-After` plus the jitter's size.
pub fn next_delay(outcome: &Outcome, interval: Duration, jitter_ms: i64) -> Duration {
    match outcome {
        Outcome::Limited(wait) => *wait + Duration::from_millis(jitter_ms.unsigned_abs()),
        _ => {
            let ms = interval.as_millis() as i64 + jitter_ms;
            Duration::from_millis(ms.max(0) as u64)
        }
    }
}

/// `Retry-After` in seconds (an HTTP date or garbage counts as the default), 1 to 300.
fn retry_after(value: Option<&str>) -> Duration {
    let secs = value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(RETRY_AFTER_DEFAULT)
        .clamp(1, RETRY_AFTER_MAX);
    Duration::from_secs(secs)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A setting the announcer cannot use (names the setting, never its value).
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The announce URL of a beholder base URL (`http(s)://host[:port][/prefix]`, no query).
pub fn announce_url(base: &str) -> Result<String, ConfigError> {
    let base = base.trim().trim_end_matches('/');
    let rest = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"));
    match rest {
        Some(r) if !r.is_empty() && !r.contains(['?', '#', '@', ' ']) => {
            Ok(format!("{base}/beholder/v1/announce"))
        }
        _ => Err(ConfigError(
            "FI_BEHOLDER_URL: an http(s)://host[:port] URL without query or user info".into(),
        )),
    }
}

/// Posts a node's announces to one beholder.
pub struct Announcer {
    url: String,
    key: NodeKey,
    client: reqwest::Client,
    interval: Duration,
    jitter: Duration,
}

impl std::fmt::Debug for Announcer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Announcer(..)")
    }
}

#[derive(Default)]
struct Counts {
    listed: u64,
    limited: u64,
    refused: u64,
    failed: u64,
    last: Option<&'static str>,
}

impl Counts {
    fn record(&mut self, kind: Kind, outcome: &Outcome) {
        match outcome {
            Outcome::Listed => self.listed += 1,
            Outcome::Limited(_) => self.limited += 1,
            Outcome::Refused(_) => self.refused += 1,
            Outcome::Failed(_) => self.failed += 1,
        }
        let label = outcome.label();
        let total = self.listed + self.limited + self.refused + self.failed;
        let changed = self.last != Some(label);
        self.last = Some(label);
        if !changed && !total.is_multiple_of(SUMMARY_EVERY) {
            return;
        }
        let (status, error) = match outcome {
            Outcome::Refused(s) => (*s, ""),
            Outcome::Limited(_) => (429, ""),
            Outcome::Failed(e) => (0, *e),
            Outcome::Listed => (204, ""),
        };
        if matches!(outcome, Outcome::Listed) {
            tracing::info!(
                kind = kind.as_str(),
                outcome = label,
                listed = self.listed,
                rate_limited = self.limited,
                refused = self.refused,
                failed = self.failed,
                "beholder announce"
            );
        } else {
            tracing::warn!(
                kind = kind.as_str(),
                outcome = label,
                status,
                error,
                listed = self.listed,
                rate_limited = self.limited,
                refused = self.refused,
                failed = self.failed,
                "beholder announce"
            );
        }
    }
}

fn jitter_ms(spread: Duration) -> i64 {
    let spread = spread.as_millis() as i64;
    if spread == 0 {
        return 0;
    }
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        return 0;
    }
    i64::from(u32::from_le_bytes(b)) % (2 * spread + 1) - spread
}

impl Announcer {
    /// An announcer for the beholder at `base` (`FI_BEHOLDER_URL`), signing with `key`, at the
    /// contract's cadence.
    pub fn new(base: &str, key: NodeKey) -> Result<Self, ConfigError> {
        let url = announce_url(base)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(TIMEOUT)
            .build()
            .map_err(|_| ConfigError("the announcer's HTTP client cannot start".into()))?;
        Ok(Self {
            url,
            key,
            client,
            interval: INTERVAL,
            jitter: JITTER,
        })
    }

    /// Another cadence and spread (tests).
    pub fn with_timing(mut self, interval: Duration, jitter: Duration) -> Self {
        self.interval = interval;
        self.jitter = jitter;
        self
    }

    /// The node's public key (`publicKey`).
    pub fn public_key(&self) -> String {
        self.key.public_key()
    }

    /// One signed post of `node`, issued now.
    pub async fn announce_once(&self, node: &Node) -> Outcome {
        let body = body(&self.key, node, now_unix()).to_string();
        let sent = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
        match sent {
            Ok(r) if r.status().as_u16() == 204 || r.status().as_u16() == 200 => Outcome::Listed,
            Ok(r) if r.status().as_u16() == 429 => Outcome::Limited(retry_after(
                r.headers().get("retry-after").and_then(|v| v.to_str().ok()),
            )),
            Ok(r) => Outcome::Refused(r.status().as_u16()),
            Err(e) if e.is_timeout() => Outcome::Failed("timeout"),
            Err(e) if e.is_connect() => Outcome::Failed("connect"),
            Err(_) => Outcome::Failed("other"),
        }
    }

    /// Announces `node` at once and then every interval until `stop` completes, with the capacity
    /// `capacity` yields before each post (clamped to 1-1000). Never returns otherwise.
    pub async fn run<C, F, S>(self, mut node: Node, mut capacity: C, stop: S)
    where
        C: FnMut() -> F,
        F: Future<Output = u32>,
        S: Future<Output = ()>,
    {
        tokio::pin!(stop);
        let mut counts = Counts::default();
        loop {
            let outcome = tokio::select! {
                o = async {
                    node.capacity = capacity().await.clamp(1, 1000);
                    self.announce_once(&node).await
                } => o,
                () = &mut stop => return,
            };
            counts.record(node.kind, &outcome);
            let delay = next_delay(&outcome, self.interval, jitter_ms(self.jitter));
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = &mut stop => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, VerifyingKey};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn server() -> Node {
        Node {
            kind: Kind::Server,
            endpoint: "https://play.example.test".into(),
            name: "Example Beta (play.example.test)".into(),
            region: String::new(),
            version: version_string("0.0.0", "local"),
            capacity: 50,
            rendezvous_port: None,
        }
    }

    fn relay() -> Node {
        Node {
            kind: Kind::Relay,
            endpoint: "203.0.113.10:3478".into(),
            name: "Example Relay 1 (203.0.113.10)".into(),
            region: "test-east".into(),
            version: "0.0.0".into(),
            capacity: 30,
            rendezvous_port: None,
        }
    }

    /// The posted bytes of a fixed announce (the key, node and instant are fixed and Ed25519 is
    /// deterministic): pinned so the move to `fi_protocol::beholder` changed nothing on the wire.
    #[test]
    fn the_announce_body_bytes_are_pinned() {
        let key = NodeKey::parse(&B64.encode([7u8; 32])).unwrap();
        assert_eq!(
            body(&key, &relay(), 1_793_000_000).to_string(),
            r#"{"capacity":30,"endpoint":"203.0.113.10:3478","issuedAt":1793000000,"kind":"relay","name":"Example Relay 1 (203.0.113.10)","publicKey":"6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw=","region":"test-east","signature":"tbj5krsBxEB4/95Ae/YVZcdfm7beshygC6/zTER3XB2FG5eWlgKLXhQlAlKfwDmv32W18IQHsdHNxZOvfE5xBQ==","version":"0.0.0"}"#
        );
    }

    /// A relay announcing its rendezvous port: one more field, and the signed message's last line.
    #[test]
    fn the_rendezvous_port_is_posted_and_signed() {
        let key = NodeKey::parse(&B64.encode([7u8; 32])).unwrap();
        let pk = key.public_key();
        let node = Node {
            rendezvous_port: Some(5101),
            ..relay()
        };
        let b = body(&key, &node, 1_793_000_000);
        assert_eq!(b.as_object().unwrap().len(), 10, "nine fields and the port");
        assert_eq!(b["rendezvousPort"], 5101);
        let m = message(&node, &pk, 1_793_000_000);
        assert_eq!(
            m,
            format!("{}\n5101", message(&relay(), &pk, 1_793_000_000))
        );
        let vk = VerifyingKey::from_bytes(&B64.decode(&pk).unwrap().try_into().unwrap()).unwrap();
        let sig =
            Signature::from_slice(&B64.decode(b["signature"].as_str().unwrap()).unwrap()).unwrap();
        vk.verify_strict(m.as_bytes(), &sig).unwrap();
        assert_ne!(
            b["signature"],
            body(&key, &relay(), 1_793_000_000)["signature"],
            "the port is covered by the signature"
        );
    }

    #[test]
    fn the_signed_message_is_the_contract_text_and_verifies() {
        let key = NodeKey::parse(&B64.encode([7u8; 32])).unwrap();
        let pk = key.public_key();
        let m = message(&relay(), &pk, 1_793_000_000);
        assert_eq!(
            m,
            format!(
                "fi-beholder-announce-v1\nrelay\n{pk}\n203.0.113.10:3478\n\
                 Example Relay 1 (203.0.113.10)\ntest-east\n0.0.0\n30\n1793000000"
            )
        );
        let b = body(&key, &relay(), 1_793_000_000);
        let fields: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(fields.len(), 9, "exactly the nine fields: {fields:?}");
        let vk = VerifyingKey::from_bytes(
            &B64.decode(b["publicKey"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap(),
        )
        .unwrap();
        let sig =
            Signature::from_slice(&B64.decode(b["signature"].as_str().unwrap()).unwrap()).unwrap();
        vk.verify_strict(m.as_bytes(), &sig).unwrap();
    }

    #[test]
    fn the_owner_names_and_endpoints_pass_the_field_rules() {
        assert_eq!(server().validate(), Ok(()));
        assert_eq!(relay().validate(), Ok(()));
        let with = |f: &dyn Fn(&mut Node)| {
            let mut n = server();
            f(&mut n);
            n.validate()
        };
        assert_eq!(with(&|n| n.name = "x".repeat(49)), Err("name"));
        assert_eq!(with(&|n| n.name = "é".repeat(48)), Ok(()));
        assert_eq!(with(&|n| n.name = String::new()), Err("name"));
        assert_eq!(with(&|n| n.name = "a\nb".into()), Err("name"));
        assert_eq!(with(&|n| n.region = "sa_east".into()), Err("region"));
        assert_eq!(with(&|n| n.capacity = 0), Err("capacity"));
        assert_eq!(with(&|n| n.capacity = 1001), Err("capacity"));
        assert_eq!(
            with(&|n| n.rendezvous_port = Some(5101)),
            Err("rendezvousPort"),
            "never on a server"
        );
        for bad in [
            "play.example.test",
            "https://play.example.test/x",
            "https://u@play.example.test",
            "https://[::1]:8080",
            "https://play.example.test:0",
        ] {
            assert_eq!(with(&|n| n.endpoint = bad.into()), Err("endpoint"), "{bad}");
        }
        assert_eq!(
            with(&|n| n.endpoint = "http://192.168.1.40:8080".into()),
            Ok(())
        );
        let mut r = relay();
        r.rendezvous_port = Some(5101);
        assert_eq!(r.validate(), Ok(()));
        r.rendezvous_port = Some(0);
        assert_eq!(r.validate(), Err("rendezvousPort"));
        r.rendezvous_port = None;
        for bad in ["203.0.113.10", "udp://203.0.113.10:3478", "1.2.3.4:99999"] {
            r.endpoint = bad.into();
            assert_eq!(r.validate(), Err("endpoint"), "{bad}");
        }
    }

    #[test]
    fn version_and_capacity_fit_their_rules() {
        assert_eq!(version_string("0.0.0", "local"), "0.0.0");
        assert_eq!(
            version_string("0.0.0", "fi-infinity-00042-abc"),
            "0.0.0+fi-infinity-00042-abc"
        );
        let long = version_string("1.2.3", "rev_with spaces/and-a-very-long-tail-000");
        assert!(long.len() <= 32 && charset(&long, b".+-"), "{long}");
        assert_eq!(clamp_capacity(None), 1000);
        assert_eq!(clamp_capacity(Some(0)), 1);
        assert_eq!(clamp_capacity(Some(24)), 24);
        assert_eq!(clamp_capacity(Some(5000)), 1000);
    }

    #[test]
    fn the_beholder_url_is_checked() {
        assert_eq!(
            announce_url("https://beholder.example.test/").unwrap(),
            "https://beholder.example.test/beholder/v1/announce"
        );
        assert!(announce_url("http://192.168.1.91:8484").is_ok());
        for bad in [
            "beholder.example.com",
            "ftp://x",
            "https://",
            "https://a?b=1",
        ] {
            assert!(announce_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn delays_follow_the_cadence_or_retry_after() {
        assert_eq!(
            next_delay(&Outcome::Listed, INTERVAL, -3000),
            Duration::from_secs(27)
        );
        assert_eq!(
            next_delay(&Outcome::Failed("connect"), INTERVAL, 3000),
            Duration::from_secs(33)
        );
        assert_eq!(
            next_delay(&Outcome::Limited(Duration::from_secs(60)), INTERVAL, -2000),
            Duration::from_secs(62)
        );
        assert_eq!(retry_after(Some("60")), Duration::from_secs(60));
        assert_eq!(retry_after(None), Duration::from_secs(60));
        assert_eq!(
            retry_after(Some("Wed, 21 Oct 2026")),
            Duration::from_secs(60)
        );
        assert_eq!(retry_after(Some("99999")), Duration::from_secs(300));
        assert_eq!(retry_after(Some("0")), Duration::from_secs(1));
        for _ in 0..1000 {
            let j = jitter_ms(JITTER);
            assert!((-3000..=3000).contains(&j));
        }
    }

    #[test]
    fn the_key_file_is_generated_once_and_reused() {
        let mut tag = [0u8; 4];
        getrandom::fill(&mut tag).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "fi-announce-key-{}-{}",
            std::process::id(),
            u32::from_le_bytes(tag)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("announce.key");
        let first = NodeKey::load_or_create(&path).unwrap();
        let second = NodeKey::load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(
            NodeKey::parse(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .public_key(),
            first.public_key()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // A broken file is reported, never replaced.
        std::fs::write(&path, "not a key").unwrap();
        assert!(matches!(
            NodeKey::load_or_create(&path),
            Err(KeyError::Invalid)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not a key");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no stray file");
        assert_eq!(format!("{first:?}"), "NodeKey(..)");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A one-request-per-connection HTTP server answering from `answers` in order (then 204),
    /// recording when each request arrived.
    async fn fake(answers: Vec<&'static str>) -> (String, Arc<Mutex<Vec<Instant>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut answers = answers.into_iter();
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let mut got = Vec::new();
                // Read the headers and the declared body.
                loop {
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if got.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                log.lock().unwrap().push(Instant::now());
                let status = answers.next().unwrap_or("204 No Content");
                let extra = if status.starts_with("429") {
                    "Retry-After: 1\r\n"
                } else {
                    ""
                };
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\n{extra}Content-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn a_429_waits_its_retry_after_and_the_loop_keeps_going() {
        let (url, _) = fake(vec!["429 Too Many Requests"]).await;
        let announcer = Announcer::new(&url, NodeKey::generate().unwrap())
            .unwrap()
            .with_timing(Duration::from_millis(50), Duration::ZERO);
        assert_eq!(
            announcer.announce_once(&relay()).await,
            Outcome::Limited(Duration::from_secs(1))
        );
        let (url, seen) = fake(vec!["429 Too Many Requests", "500 Internal Server Error"]).await;
        let announcer = Announcer::new(&url, NodeKey::generate().unwrap())
            .unwrap()
            .with_timing(Duration::from_millis(50), Duration::ZERO);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(announcer.run(relay(), || async { 30 }, async {
            let _ = rx.await;
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().len() < 4 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = tx.send(());
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the loop stops on shutdown")
            .unwrap();
        let at = seen.lock().unwrap().clone();
        assert!(
            at.len() >= 4,
            "kept announcing after 429 and 500: {}",
            at.len()
        );
        assert!(
            at[1] - at[0] >= Duration::from_secs(1),
            "429 waited Retry-After"
        );
        assert!(
            at[2] - at[1] < Duration::from_millis(900),
            "500 kept the cadence"
        );
        assert!(
            at[3] - at[2] < Duration::from_millis(900),
            "204 kept the cadence"
        );
    }

    #[tokio::test]
    async fn an_unreachable_beholder_is_a_failure_not_a_stop() {
        // A port nothing listens on.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let a = Announcer::new(&url, NodeKey::generate().unwrap()).unwrap();
        assert!(matches!(
            a.announce_once(&server()).await,
            Outcome::Failed(_)
        ));
    }
}
