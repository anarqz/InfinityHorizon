//! The rendezvous socket loop with the detector beside it (MATCHMAKING_API.md §5d). The loop is
//! `fi_relay::udp::serve_rendezvous`'s, built from fi-relay's public pieces (crates/fi-relay is not
//! changed): every datagram goes through `Rendezvous::handle` exactly as before and its answers are
//! sent first; the [`Observer`] only reads what crossed:
//!
//! - a `hello` whose answer is a `peer` pair registered both sides of a grant: its claims are
//!   verified again (one Ed25519 check per pairing, never per datagram) and the grant's world,
//!   joiner and host XUIDs and both endpoints (taken from the `peer` datagrams themselves) are kept
//!   until the grant's `exp` (extended by every fresher pairing hello);
//! - a `data` datagram that was forwarded (its answer is not empty): its payload is fed to the
//!   detector with the leg it crossed (from the joiner's endpoint: joiner → host).
//!
//! Forwarding never waits on detection: findings leave over a bounded channel (`try_send`; full is
//! counted as dropped) and the rules arrive over a watch channel read once a second.

use fi_netwatch::detect::{Counters, Finding, GrantId, GrantInfo, Leg};
use fi_netwatch::{Detector, Rules};
use fi_relay::rendezvous::Out;
use fi_relay::udp::SourceLimit;
use fi_relay::wire::{self, Message, Role};
use fi_relay::{Rendezvous, VerifyKeys};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

/// Largest datagram read (as fi-relay).
const READ_BUF: usize = 2048;
/// Grants whose pairing is remembered at once (fi-relay's default `max_grants`).
pub const MAX_PAIRS: usize = 4096;

/// Unix milliseconds now.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug)]
struct Pair {
    info: GrantInfo,
    joiner: SocketAddr,
    host: SocketAddr,
}

/// What the detector knows of the relay's grants, and the detector.
#[derive(Debug)]
pub struct Observer {
    keys: VerifyKeys,
    pairs: HashMap<GrantId, Pair>,
    /// The detector (public for the loopback tests).
    pub detector: Detector,
}

impl Observer {
    /// An observer verifying hellos with `keys` (the relay's `FI_RELAY_GRANT_VERIFY_KEYS`).
    pub fn new(keys: VerifyKeys, detector: Detector) -> Self {
        Self {
            keys,
            pairs: HashMap::with_capacity(MAX_PAIRS.min(1024)),
            detector,
        }
    }

    /// Grants paired now.
    #[cfg(test)]
    pub fn pairs(&self) -> usize {
        self.pairs.len()
    }

    /// One datagram `data` from `from` that the rendezvous answered with `out`.
    pub fn on_datagram(&mut self, from: SocketAddr, data: &[u8], out: &Out, now_ms: u64) {
        match Message::parse(data) {
            Some(Message::Hello {
                claims, signature, ..
            }) if !out.is_empty() => self.paired(claims, &signature, out, now_ms),
            Some(Message::Data { grant, payload }) if !out.is_empty() => {
                let Some(p) = self.pairs.get(&grant).copied() else {
                    return;
                };
                if p.info.exp <= now_ms / 1000 {
                    return;
                }
                let leg = if from == p.joiner {
                    Leg::JoinerToHost
                } else if from == p.host {
                    Leg::HostToJoiner
                } else {
                    return;
                };
                self.detector.observe(&grant, p.info, leg, payload, now_ms);
            }
            _ => {}
        }
    }

    fn paired(&mut self, claims: &[u8], signature: &[u8; 64], out: &Out, now_ms: u64) {
        // Only a registering hello with both sides known answers a `peer` pair.
        let mut joiner = None;
        let mut host = None;
        let mut id = None;
        for (to, bytes) in out {
            if let Some(Message::Peer {
                grant,
                role,
                endpoint,
            }) = Message::parse(bytes)
            {
                id = Some(grant);
                match role {
                    // Sent to the joiner, naming the host.
                    Role::Host => {
                        joiner = Some(*to);
                        host = Some(endpoint);
                    }
                    Role::Joiner => {
                        host = Some(*to);
                        joiner = Some(endpoint);
                    }
                }
            }
        }
        let (Some(id), Some(joiner), Some(host)) = (id, joiner, host) else {
            return;
        };
        let Ok(c) = self
            .keys
            .verify_raw(claims, signature, (now_ms / 1000) as i64)
        else {
            return;
        };
        let (Some(world), Ok(jx), Ok(hx)) = (
            wire::grant_id(&c.world),
            c.xuid.parse::<u64>(),
            c.host.xuid().unwrap_or_default().parse::<u64>(),
        ) else {
            return;
        };
        if wire::grant_id(&c.grant) != Some(id) {
            return;
        }
        let exp = u64::try_from(c.exp).unwrap_or(0);
        if !self.pairs.contains_key(&id) && self.pairs.len() >= MAX_PAIRS {
            let now_s = now_ms / 1000;
            self.pairs.retain(|_, p| p.info.exp > now_s);
            if self.pairs.len() >= MAX_PAIRS {
                return;
            }
        }
        let entry = self.pairs.entry(id).or_insert(Pair {
            info: GrantInfo {
                world,
                joiner: jx,
                host: hx,
                exp,
            },
            joiner,
            host,
        });
        entry.info.exp = entry.info.exp.max(exp);
        entry.joiner = joiner;
        entry.host = host;
    }

    /// The once-a-second step: expired pairings dropped, the detector ticked.
    pub fn tick(&mut self, now_ms: u64, out: &mut Vec<Finding>) {
        let now_s = now_ms / 1000;
        self.pairs.retain(|_, p| p.info.exp > now_s);
        self.detector.tick(now_ms, out);
    }
}

/// The detector's links to the reporting task.
#[derive(Debug)]
pub struct Links {
    /// Findings out (bounded).
    pub findings: mpsc::Sender<Finding>,
    /// The detector's counters, refreshed each second.
    pub counters: watch::Sender<Counters>,
    /// The rules in.
    pub rules: watch::Receiver<Option<Rules>>,
    /// Reports dropped (shared with the reporter).
    pub dropped: Arc<AtomicU64>,
}

async fn send_all(socket: &UdpSocket, out: &Out) {
    for (to, bytes) in out {
        if socket.send_to(bytes, to).await.is_err() {
            tracing::debug!("relay send failed");
        }
    }
}

/// `fi_relay::udp::serve_rendezvous` with the observer: runs until `shutdown`, returns the state.
pub async fn serve(
    socket: Arc<UdpSocket>,
    mut relay: Rendezvous,
    mut observer: Observer,
    mut links: Links,
    shutdown: impl Future<Output = ()>,
) -> (Rendezvous, Observer) {
    let mut buf = [0u8; READ_BUF];
    let mut limit = SourceLimit::default();
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    let mut stats = tokio::time::interval(Duration::from_secs(60));
    let mut second = tokio::time::interval(Duration::from_secs(1));
    second.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut findings: Vec<Finding> = Vec::with_capacity(64);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return (relay, observer),
            _ = sweep.tick() => {
                relay.sweep((now_ms() / 1000) as i64);
            }
            _ = stats.tick() => {
                let s = relay.stats();
                let c = observer.detector.counters();
                tracing::info!(
                    tracked = relay.tracked(),
                    relayed = relay.relayed(),
                    hellos = s.hellos,
                    ignored = s.ignored,
                    refused = s.refused,
                    forwarded = s.forwarded,
                    decoded = c.decoded,
                    rings = c.rings_in_use,
                    "relay"
                );
            }
            _ = second.tick() => {
                if links.rules.has_changed().unwrap_or(false) {
                    let rules = *links.rules.borrow_and_update();
                    observer.detector.set_rules(rules);
                }
                findings.clear();
                observer.tick(now_ms(), &mut findings);
                for f in findings.drain(..) {
                    if links.findings.try_send(f).is_err() {
                        links.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let _ = links.counters.send(observer.detector.counters());
            }
            read = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = read else { continue };
                if let Some(answer) = wire::binding_answer(&buf[..n], from) {
                    if limit.allow(from, now_ms() / 1000) {
                        let _ = socket.send_to(&answer, from).await;
                    }
                    continue;
                }
                let now = now_ms();
                let out = relay.handle(from, &buf[..n], (now / 1000) as i64);
                send_all(&socket, &out).await;
                observer.on_datagram(from, &buf[..n], &out, now);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use fi_netwatch::detect::{FindingType, Kind, Limit};
    use fi_netwatch::nerd::synth::{car, packet};
    use fi_netwatch::nerd::{Handle, HandleKind};
    use fi_relay::Config;

    pub const JOINER_XUID: u64 = 2_535_100_000_000_002;
    pub const HOST_XUID: u64 = 2_535_100_000_000_001;
    pub const WORLD: &str = "0192f0c4-0000-7000-8000-0000000000aa";

    pub fn grant_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub fn verify_keys(seed: u8) -> VerifyKeys {
        VerifyKeys::new(vec![(
            "rg-1".into(),
            grant_key(seed).verifying_key().to_bytes(),
        )])
        .unwrap()
    }

    /// A signed hello of `role` for `grant` (claims as JSON: the base claim set).
    pub fn hello(
        seed: u8,
        grant: &str,
        joiner: u64,
        role: Role,
        now: i64,
        cookie: Option<[u8; 16]>,
    ) -> Vec<u8> {
        let claims = serde_json::to_vec(&serde_json::json!({
            "v": 1, "kid": "rg-1", "grant": grant, "world": WORLD,
            "sub": "0192f0c4-0000-7000-8000-0000000000bb", "xuid": joiner.to_string(), "tier": "free",
            "host": {"xuid": HOST_XUID.to_string()}, "role": role.claim(), "iat": now, "exp": now + 60
        }))
        .unwrap();
        let mut m = fi_protocol::matchmaking::RELAY_GRANT_PREFIX.to_vec();
        m.extend_from_slice(&claims);
        let signature = grant_key(seed).sign(&m).to_bytes();
        Message::Hello {
            role,
            claims: &claims,
            signature,
            cookie,
        }
        .encode()
    }

    pub async fn recv(s: &UdpSocket) -> Option<Vec<u8>> {
        let mut buf = [0u8; READ_BUF];
        match tokio::time::timeout(Duration::from_millis(500), s.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
            _ => None,
        }
    }

    /// Hello, the challenge, hello with the cookie.
    pub async fn say_hello(s: &UdpSocket, relay: SocketAddr, grant: &str, joiner: u64, role: Role) {
        let now = (now_ms() / 1000) as i64;
        s.send_to(&hello(7, grant, joiner, role, now, None), relay)
            .await
            .unwrap();
        let c = recv(s).await.expect("a challenge");
        let Some(Message::Challenge { cookie, .. }) = Message::parse(&c) else {
            panic!("not a challenge")
        };
        s.send_to(&hello(7, grant, joiner, role, now, Some(cookie)), relay)
            .await
            .unwrap();
    }

    pub fn fast_rules() -> Rules {
        Rules {
            version: 1,
            speed: Limit {
                max_kmh: 451.0,
                sustain_ms: 1000,
            },
            climb: Limit {
                max_kmh: 150.0,
                sustain_ms: 1000,
            },
            jump_m: 200.0,
        }
    }

    /// Loopback: a joiner driving at 500 km/h through the relay is reported, every datagram is
    /// forwarded byte for byte, and nothing reaches the detector before the grant is relayed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_relayed_joiner_over_the_limit_is_reported_and_forwarding_is_unchanged() {
        let relay_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let relay_addr = relay_sock.local_addr().unwrap();
        let config = Config {
            fallback: true,
            ..Config::default()
        };
        let detector = Detector::new(fi_netwatch::Config { max_peers: 24 }, 1);
        let observer = Observer::new(verify_keys(7), detector);
        let (ftx, mut frx) = mpsc::channel(64);
        let (ctx, crx) = watch::channel(Counters::default());
        let (rtx, rrx) = watch::channel(None);
        rtx.send(Some(fast_rules())).unwrap();
        let links = Links {
            findings: ftx,
            counters: ctx,
            rules: rrx,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(serve(
            relay_sock,
            Rendezvous::new(verify_keys(7), config),
            observer,
            links,
            async move {
                let _ = stop_rx.await;
            },
        ));
        let joiner = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let host = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let g = "0192f0c4-0000-7000-8000-000000000001";
        let id = wire::grant_id(g).unwrap();
        say_hello(&joiner, relay_addr, g, JOINER_XUID, Role::Joiner).await;
        say_hello(&host, relay_addr, g, JOINER_XUID, Role::Host).await;
        assert!(
            recv(&joiner).await.is_some() && recv(&host).await.is_some(),
            "peer pair"
        );
        joiner
            .send_to(&Message::Relay { grant: id }.encode(), relay_addr)
            .await
            .unwrap();
        assert!(
            recv(&joiner).await.is_some() && recv(&host).await.is_some(),
            "relay ok"
        );
        let h = Handle {
            kind: HandleKind::Own,
            id: 4,
            owner: 0,
        };
        let (mut x, mut stamp) = (0.0f64, 100_000u32);
        let mut found = None;
        for _ in 0..200 {
            x += 500.0 / 3.6 * 0.05;
            stamp += 50;
            let payload = packet(
                1,
                &[car(
                    h,
                    [x as f32, 0.0, 0.0],
                    [500.0 / 3.6, 0.0, 0.0],
                    0,
                    stamp,
                )],
            );
            let dgram = Message::Data {
                grant: id,
                payload: &payload,
            }
            .encode();
            joiner.send_to(&dgram, relay_addr).await.unwrap();
            assert_eq!(
                recv(&host).await.as_deref(),
                Some(&dgram[..]),
                "forwarded byte for byte"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
            if let Ok(f) = frx.try_recv() {
                found = Some(f);
                break;
            }
        }
        let f = match found {
            Some(f) => f,
            None => tokio::time::timeout(Duration::from_secs(6), frx.recv())
                .await
                .expect("a finding")
                .unwrap(),
        };
        assert_eq!((f.kind_type, f.kind), (FindingType::Violation, Kind::Speed));
        assert_eq!((f.xuid, f.host_xuid), (JOINER_XUID, HOST_XUID));
        assert_eq!(f.world, wire::grant_id(WORLD).unwrap());
        assert!(crx.borrow().decoded > 10);
        stop_tx.send(()).unwrap();
        let (relay, observer) = task.await.unwrap();
        assert_eq!(relay.relayed(), 1);
        assert_eq!(observer.pairs(), 1);
    }
}
