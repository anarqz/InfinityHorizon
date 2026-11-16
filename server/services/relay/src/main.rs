//! fi-relay-vm: the relay VM's process (ADR 0017 §2, MATCHMAKING_API.md §5). Binds the binding
//! port and the rendezvous port, runs `fi_relay` on them, and stops on SIGTERM or Ctrl-C (systemd
//! and Docker restart it). It reads only:
//!
//! | Variable | Default | What |
//! | --- | --- | --- |
//! | `FI_RELAY_GRANT_VERIFY_KEYS` | required | `kid:base64(32-byte public key)`, from instance metadata |
//! | `FI_RELAY_FALLBACK` | `false` | forward for peers that could not punch |
//! | `FI_RELAY_MAX_PEERS` | `24` | grants forwarded at once |
//! | `FI_RELAY_BIND` | `0.0.0.0` | listen address |
//! | `FI_RELAY_BINDING_PORT`, `FI_RELAY_PORT` | `3478`, `5101` | UDP ports (`0`: any free port) |
//! | `FI_RELAY_PLATFORM_URL` | unset (no detection) | the play origin `https://host[:port]` the anti-cheat detector reports to (MATCHMAKING_API.md §5d); needs the node key below |
//!
//! **The anti-cheat detector** (§5d, OD-160, crates/fi-netwatch) runs only with
//! `FI_RELAY_PLATFORM_URL` **and** a node key: the rendezvous loop ([`observe`]) feeds every
//! forwarded `data` payload of a relayed grant to the detector, whose rings, owner maps and outbox
//! are allocated once at start (min(3 × `FI_RELAY_MAX_PEERS`, 3000) rings); the reporter
//! ([`platform`]) fetches `WatchRules` and pushes signed report batches with a per-start key
//! certified by the node key. Forwarding never waits on it; nothing it holds is logged or written.
//!
//! Announcing to the beholder (ANNOUNCER_API.md, CR-S54) is optional, on only with `FI_BEHOLDER_URL`:
//!
//! | Variable | Default | What |
//! | --- | --- | --- |
//! | `FI_BEHOLDER_URL` | unset (off) | the beholder's base URL, `https://beholder.<domain>` |
//! | `FI_ANNOUNCE_ENDPOINT` | required when on | `host:port` launchers reach (public address, binding port) |
//! | `FI_ANNOUNCE_NAME` | the endpoint's host | the listing's name, 1-48 characters |
//! | `FI_ANNOUNCE_REGION` | empty | 0-16 of `[A-Za-z0-9-]` |
//! | `FI_ANNOUNCE_SIGNING_KEY` | unset | the node key (base64 of a 32-byte Ed25519 seed) |
//! | `FI_ANNOUNCE_KEY_FILE` | unset | a file holding it, generated once (0600) when missing |
//!
//! The node key also identifies this relay in grants (CR-S57, MATCHMAKING_API.md §5b): when
//! `FI_ANNOUNCE_SIGNING_KEY` or `FI_ANNOUNCE_KEY_FILE` is set (with or without a beholder), a
//! grant whose signed `relay` claim names another key is ignored; a relay without a node key
//! ignores every grant naming a relay (grants without the claim work as before).
//!
//! The capacity announced is `FI_RELAY_MAX_PEERS` (1-1000) and `rendezvousPort` is the port the
//! rendezvous socket bound (`FI_RELAY_PORT`, 5101 by default; a relay behind a port mapping that
//! changes it would announce the wrong port). The announce variables come from the
//! environment only, never from `FI_KEYS_DIR`, so a relay beside a server never takes the server's
//! key (one key, one listing).
//!
//! No database and no FI secret: a relay compromise yields no credential (the announce key only
//! identifies this node to the directory). Logs carry counts, never an address, a key or a payload.

mod observe;
mod platform;

use fi_announce::{Announcer, Kind, Node, NodeKey};
use fi_relay::{Config, Rendezvous, VerifyKeys, udp};
use fi_runtime::{ServiceInfo, http, telemetry};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UdpSocket;

/// The parsed settings.
#[derive(Debug)]
struct Settings {
    keys: VerifyKeys,
    config: Config,
    bind: IpAddr,
    binding_port: u16,
    port: u16,
    /// `FI_RELAY_PLATFORM_URL` (§5d): where the detector reports; `None`: no detection.
    platform: Option<String>,
}

/// Where the node key comes from.
#[derive(Debug, PartialEq, Eq)]
enum KeySource {
    /// `FI_ANNOUNCE_SIGNING_KEY`.
    Value,
    /// `FI_ANNOUNCE_KEY_FILE`.
    File(PathBuf),
}

/// The announcer's settings, when `FI_BEHOLDER_URL` is set.
#[derive(Debug)]
struct Announce {
    url: String,
    node: Node,
    key: KeySource,
}

/// The announce settings from the process environment only (`get`): `None` without
/// `FI_BEHOLDER_URL`. `max_peers` is the capacity announced.
fn announce_settings(
    get: impl Fn(&str) -> Option<String>,
    max_peers: usize,
) -> Result<Option<Announce>, String> {
    let get = |name: &str| {
        get(name)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    let Some(url) = get("FI_BEHOLDER_URL") else {
        return Ok(None);
    };
    fi_announce::announce_url(&url).map_err(|e| e.to_string())?;
    let endpoint = get("FI_ANNOUNCE_ENDPOINT").ok_or(
        "FI_ANNOUNCE_ENDPOINT is required with FI_BEHOLDER_URL (public host:binding port)",
    )?;
    let host = fi_announce::endpoint_host(Kind::Relay, &endpoint)
        .ok_or("FI_ANNOUNCE_ENDPOINT: host:port (a DNS name or an IPv4 address)")?;
    let node = Node {
        kind: Kind::Relay,
        name: get("FI_ANNOUNCE_NAME").unwrap_or_else(|| host.chars().take(48).collect()),
        region: get("FI_ANNOUNCE_REGION").unwrap_or_default(),
        version: fi_announce::version_string(env!("CARGO_PKG_VERSION"), ""),
        capacity: fi_announce::clamp_capacity(Some(max_peers as u64)),
        endpoint,
        // The port the rendezvous socket actually binds, set once it is bound (`run`).
        rendezvous_port: None,
    };
    node.validate().map_err(|field| match field {
        "name" => "FI_ANNOUNCE_NAME: 1-48 characters, no control character",
        "region" => "FI_ANNOUNCE_REGION: 0-16 of A-Z, a-z, 0-9 and -",
        _ => "FI_ANNOUNCE_ENDPOINT: host:port",
    })?;
    let key = if get("FI_ANNOUNCE_SIGNING_KEY").is_some() {
        KeySource::Value
    } else if let Some(file) = get("FI_ANNOUNCE_KEY_FILE") {
        KeySource::File(PathBuf::from(file))
    } else {
        return Err(
            "FI_BEHOLDER_URL needs FI_ANNOUNCE_SIGNING_KEY or FI_ANNOUNCE_KEY_FILE (the node key)"
                .into(),
        );
    };
    Ok(Some(Announce { url, node, key }))
}

/// The node key: the variable, or the file (generated once when missing).
fn node_key(source: &KeySource) -> Result<NodeKey, String> {
    match source {
        KeySource::Value => {
            NodeKey::parse(&std::env::var("FI_ANNOUNCE_SIGNING_KEY").unwrap_or_default())
                .map_err(|_| "FI_ANNOUNCE_SIGNING_KEY: base64 of 32 bytes".to_owned())
        }
        KeySource::File(path) => {
            NodeKey::load_or_create(path).map_err(|e| format!("FI_ANNOUNCE_KEY_FILE: {e}"))
        }
    }
}

fn settings(get: impl Fn(&str) -> Option<String>) -> Result<Settings, String> {
    let get = |name: &str| get(name).filter(|v| !v.trim().is_empty());
    let keys = VerifyKeys::parse(
        "FI_RELAY_GRANT_VERIFY_KEYS",
        &get("FI_RELAY_GRANT_VERIFY_KEYS").ok_or("FI_RELAY_GRANT_VERIFY_KEYS is required")?,
    )
    .map_err(|e| e.to_string())?;
    let fallback = match get("FI_RELAY_FALLBACK").as_deref().map(str::trim) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err("FI_RELAY_FALLBACK: true or false".into()),
    };
    let number = |name: &str, default: u16| -> Result<u16, String> {
        get(name)
            .map(|v| {
                v.trim()
                    .parse::<u16>()
                    .map_err(|_| format!("{name}: a number"))
            })
            .unwrap_or(Ok(default))
    };
    let max_peers = number("FI_RELAY_MAX_PEERS", 24)?;
    let bind: IpAddr = get("FI_RELAY_BIND")
        .unwrap_or_else(|| "0.0.0.0".into())
        .trim()
        .parse()
        .map_err(|_| "FI_RELAY_BIND: an IP address".to_owned())?;
    Ok(Settings {
        keys,
        config: Config {
            fallback,
            max_peers: usize::from(max_peers),
            ..Config::default()
        },
        bind,
        binding_port: number("FI_RELAY_BINDING_PORT", 3478)?,
        port: number("FI_RELAY_PORT", 5101)?,
        platform: get("FI_RELAY_PLATFORM_URL")
            .map(|v| platform::platform_url(&v))
            .transpose()?,
    })
}

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("fi-relay-vm: cannot start the async runtime: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fi-relay-vm: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The node key's source from the process environment alone, with or without a beholder
/// (CR-S57: the relay accepts a grant naming a relay only when it names this node's key).
fn own_key_source(get: impl Fn(&str) -> Option<String>) -> Option<KeySource> {
    let get = |name: &str| get(name).filter(|v| !v.trim().is_empty());
    if get("FI_ANNOUNCE_SIGNING_KEY").is_some() {
        Some(KeySource::Value)
    } else {
        get("FI_ANNOUNCE_KEY_FILE").map(|f| KeySource::File(PathBuf::from(f.trim())))
    }
}

/// The 32 bytes of a node key's public half.
fn public_bytes(key: &NodeKey) -> Option<[u8; 32]> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(key.public_key())
        .ok()?;
    <[u8; 32]>::try_from(bytes.as_slice()).ok()
}

async fn run() -> Result<(), String> {
    // A standalone instance's relay reads its public ring from the keys volume (FI_KEYS_DIR).
    let mut s = settings(fi_runtime::config::secret)?;
    // CR-S57: grants naming a relay must name this node's key; without a key, none is accepted.
    let mut own_key = None;
    if let Some(source) = own_key_source(|n| std::env::var(n).ok()) {
        let key = node_key(&source)?;
        s.config.node_key = public_bytes(&key);
        own_key = Some(key);
    }
    // §5d: the detector runs with a platform URL and a node key (the key certifies its reports).
    let detection = match (&s.platform, &own_key) {
        (Some(base), Some(key)) => Some(detector_parts(base, key, &s)?),
        (Some(_), None) => {
            eprintln!("fi-relay-vm: FI_RELAY_PLATFORM_URL without a node key: no detection");
            None
        }
        _ => None,
    };
    // The process environment only: never the keys directory (see the module's documentation).
    let mut announce = announce_settings(|n| std::env::var(n).ok(), s.config.max_peers)?;
    let announcer = match &announce {
        Some(a) => Some(Announcer::new(&a.url, node_key(&a.key)?).map_err(|e| e.to_string())?),
        None => None,
    };
    let info = ServiceInfo::from_env("fi-relay-vm", env!("CARGO_PKG_VERSION"))
        .with_label("fallback", s.config.fallback.to_string())
        .with_label("max_peers", s.config.max_peers.to_string())
        .with_label("detector", detection.is_some().to_string());
    telemetry::init(&info);
    let binding = UdpSocket::bind((s.bind, s.binding_port))
        .await
        .map_err(|e| format!("binding port: {e}"))?;
    let rendezvous = UdpSocket::bind((s.bind, s.port))
        .await
        .map_err(|e| format!("rendezvous port: {e}"))?;
    let (b, r) = (
        binding.local_addr().map_err(|e| e.to_string())?,
        rendezvous.local_addr().map_err(|e| e.to_string())?,
    );
    // CR-S57 follow-up: the listing's `rendezvousPort` is the port this socket bound (the NAT
    // test's second binding request goes there).
    if let Some(a) = announce.as_mut() {
        a.node.rendezvous_port = Some(r.port());
    }
    tracing::info!(
        binding_port = b.port(),
        rendezvous_port = r.port(),
        fallback = s.config.fallback,
        max_peers = s.config.max_peers,
        "relay listening"
    );
    println!(
        "fi-relay-vm listening binding={} rendezvous={}",
        b.port(),
        r.port()
    );
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|v| *v).await;
    };
    let binding_task = tokio::spawn(udp::serve_binding(
        Arc::new(binding),
        stopped(stop_rx.clone()),
    ));
    let announce_task = match (announce, announcer) {
        (Some(a), Some(announcer)) => {
            tracing::info!(name = %a.node.name, "announcing to the beholder");
            let capacity = a.node.capacity;
            Some(tokio::spawn(announcer.run(
                a.node,
                move || async move { capacity },
                stopped(stop_rx.clone()),
            )))
        }
        _ => None,
    };
    let (relay_task, report_task) = match detection {
        None => (
            tokio::spawn(async move {
                udp::serve_rendezvous(
                    Arc::new(rendezvous),
                    Rendezvous::new(s.keys, s.config),
                    stopped(stop_rx),
                )
                .await;
            }),
            None,
        ),
        Some((p, detector)) => {
            tracing::info!(rings = detector.capacity(), "anti-cheat detector on");
            let (findings_tx, findings_rx) = tokio::sync::mpsc::channel(platform::OUTBOX);
            let (counters_tx, counters_rx) = tokio::sync::watch::channel(Default::default());
            let (rules_tx, rules_rx) = tokio::sync::watch::channel(None);
            let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let links = observe::Links {
                findings: findings_tx,
                counters: counters_tx,
                rules: rules_rx,
                dropped: dropped.clone(),
            };
            let inputs = platform::Inputs {
                findings: findings_rx,
                counters: counters_rx,
                rules: rules_tx,
                dropped,
            };
            let report = tokio::spawn(platform::run(p, inputs, stopped(stop_rx.clone())));
            let observer = observe::Observer::new(s.keys.clone(), detector);
            let relay = tokio::spawn(async move {
                observe::serve(
                    Arc::new(rendezvous),
                    Rendezvous::new(s.keys, s.config),
                    observer,
                    links,
                    stopped(stop_rx),
                )
                .await;
            });
            (relay, Some(report))
        }
    };
    http::shutdown_signal().await;
    let _ = stop_tx.send(true);
    let _ = binding_task.await;
    let _ = relay_task.await;
    if let Some(task) = report_task {
        let _ = task.await;
    }
    if let Some(task) = announce_task {
        let _ = task.await;
    }
    Ok(())
}

/// The reporter and the detector, sized from the settings (allocated here, once).
fn detector_parts(
    base: &str,
    key: &NodeKey,
    s: &Settings,
) -> Result<(platform::Platform, fi_netwatch::Detector), String> {
    let seed = fi_netwatch::cert::key32(&key.to_text()).ok_or("node key: base64 of 32 bytes")?;
    let issuers = platform::issuers(
        &fi_runtime::config::secret("FI_RELAY_GRANT_VERIFY_KEYS").unwrap_or_default(),
    )?;
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("platform client: {e}"))?;
    let mut rng = [0u8; 8];
    getrandom::fill(&mut rng).map_err(|_| "the system random source is unavailable")?;
    Ok((
        platform::Platform {
            base: base.to_owned(),
            node: ed25519_dalek::SigningKey::from_bytes(&seed),
            issuers,
            http,
        },
        fi_netwatch::Detector::new(
            fi_netwatch::Config {
                max_peers: s.config.max_peers,
            },
            u64::from_le_bytes(rng),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "rg-local-1:A4Qtlcgz2EIO20E32DMyuO5ss4tu84VRWw51EeB04wg=";

    #[test]
    fn settings_need_only_the_public_key() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let s = settings(env(&[("FI_RELAY_GRANT_VERIFY_KEYS", KEY)])).unwrap();
        assert_eq!(
            (
                s.binding_port,
                s.port,
                s.config.fallback,
                s.config.max_peers
            ),
            (3478, 5101, false, 24)
        );
        assert!(settings(env(&[])).is_err());
        assert!(
            settings(env(&[
                ("FI_RELAY_GRANT_VERIFY_KEYS", KEY),
                ("FI_RELAY_FALLBACK", "yes")
            ]))
            .is_err()
        );
        let s = settings(env(&[
            ("FI_RELAY_GRANT_VERIFY_KEYS", KEY),
            ("FI_RELAY_FALLBACK", "true"),
            ("FI_RELAY_MAX_PEERS", "8"),
        ]))
        .unwrap();
        assert_eq!((s.config.fallback, s.config.max_peers), (true, 8));
    }

    fn vars(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn announcing_is_off_without_a_beholder_url() {
        assert!(announce_settings(vars(&[]), 24).unwrap().is_none());
        assert!(
            announce_settings(vars(&[("FI_ANNOUNCE_ENDPOINT", "1.2.3.4:3478")]), 24)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_cloud_relay_announces_with_the_owner_name() {
        let a = announce_settings(
            vars(&[
                ("FI_BEHOLDER_URL", "https://beholder.example.test"),
                ("FI_ANNOUNCE_ENDPOINT", "203.0.113.10:3478"),
                ("FI_ANNOUNCE_NAME", "Example Relay 1 (203.0.113.10)"),
                ("FI_ANNOUNCE_REGION", "test-east"),
                ("FI_ANNOUNCE_KEY_FILE", "/var/lib/fi-relay/announce.key"),
            ]),
            30,
        )
        .unwrap()
        .unwrap();
        assert_eq!(a.node.kind, Kind::Relay);
        assert_eq!(a.node.name, "Example Relay 1 (203.0.113.10)");
        assert_eq!(a.node.capacity, 30);
        assert_eq!(
            a.key,
            KeySource::File("/var/lib/fi-relay/announce.key".into())
        );
        // The endpoint's host is the default name; 0 peers announce 1.
        let a = announce_settings(
            vars(&[
                ("FI_BEHOLDER_URL", "https://beholder.example.com"),
                ("FI_ANNOUNCE_ENDPOINT", "relay.example.com:3478"),
                ("FI_ANNOUNCE_SIGNING_KEY", "x"),
            ]),
            0,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (a.node.name.as_str(), a.node.capacity),
            ("relay.example.com", 1)
        );
        assert_eq!(a.key, KeySource::Value);
    }

    #[test]
    fn announcing_refuses_a_bad_setting_at_start() {
        let base = |extra: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                [
                    ("FI_BEHOLDER_URL", "https://beholder.example.com"),
                    ("FI_ANNOUNCE_KEY_FILE", "/tmp/k"),
                ]
                .iter()
                .chain(extra.iter())
                .rev()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
            }
        };
        for (extra, word) in [
            (&[][..], "FI_ANNOUNCE_ENDPOINT"),
            (
                &[("FI_ANNOUNCE_ENDPOINT", "203.0.113.10")][..],
                "FI_ANNOUNCE_ENDPOINT",
            ),
            (
                &[("FI_ANNOUNCE_ENDPOINT", "http://203.0.113.10:3478")][..],
                "FI_ANNOUNCE_ENDPOINT",
            ),
            (
                &[
                    ("FI_ANNOUNCE_ENDPOINT", "1.2.3.4:3478"),
                    (
                        "FI_ANNOUNCE_NAME",
                        "a name far longer than the forty-eight characters",
                    ),
                ][..],
                "FI_ANNOUNCE_NAME",
            ),
            (
                &[
                    ("FI_ANNOUNCE_ENDPOINT", "1.2.3.4:3478"),
                    ("FI_BEHOLDER_URL", "beholder"),
                ][..],
                "FI_BEHOLDER_URL",
            ),
            (
                &[
                    ("FI_ANNOUNCE_ENDPOINT", "1.2.3.4:3478"),
                    ("FI_ANNOUNCE_KEY_FILE", " "),
                ][..],
                "FI_ANNOUNCE_SIGNING_KEY",
            ),
        ] {
            let e = announce_settings(base(extra), 24).unwrap_err();
            assert!(e.contains(word), "{e}");
        }
    }

    fn fi_relay_b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// CR-S57: the node key identifies the relay in grants with or without a beholder.
    #[test]
    fn the_node_key_is_read_without_a_beholder_too() {
        assert_eq!(own_key_source(vars(&[])), None);
        assert_eq!(
            own_key_source(vars(&[(
                "FI_ANNOUNCE_KEY_FILE",
                "/var/lib/fi-relay/announce.key"
            )])),
            Some(KeySource::File(PathBuf::from(
                "/var/lib/fi-relay/announce.key"
            )))
        );
        assert_eq!(
            own_key_source(vars(&[
                ("FI_ANNOUNCE_SIGNING_KEY", "x"),
                ("FI_ANNOUNCE_KEY_FILE", "/k")
            ])),
            Some(KeySource::Value)
        );
        let key = NodeKey::generate().unwrap();
        let bytes = public_bytes(&key).unwrap();
        assert_eq!(fi_relay_b64(&bytes), key.public_key());
    }
}
