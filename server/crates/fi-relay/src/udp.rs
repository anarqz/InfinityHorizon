//! The two sockets of a relay: the binding port answers Binding Requests statelessly, at most
//! [`BINDING_PER_SECOND`] per source network and second ([`SourceLimit`]: the answer is larger
//! than the request, so an unlimited responder would be a reflector against third parties and
//! burn the egress budget); the rendezvous port feeds one [`Rendezvous`] owned by a single task
//! (no lock), which also prunes expired grants every few seconds. Send errors are counted and
//! dropped: UDP is best effort and the peers retry. Nothing here logs an address or a payload.

use crate::rendezvous::{Out, Rendezvous};
use crate::wire;
use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::UdpSocket;

/// Largest datagram read (anything longer is truncated by the OS and then fails to parse).
const READ_BUF: usize = 2048;

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

async fn send_all(socket: &UdpSocket, out: Out) {
    for (to, bytes) in out {
        if socket.send_to(&bytes, to).await.is_err() {
            tracing::debug!("relay send failed");
        }
    }
}

/// Binding answers per source network (/24 or /48) and second. A launcher sends a handful per
/// session; a NAT shared by many players stays far below it.
pub const BINDING_PER_SECOND: u32 = 20;
/// Most source networks counted in one second; beyond it new networks wait for the next second.
pub const MAX_SOURCES: usize = 16_384;

/// Fixed one-second windows per source network, owned by the binding task (no lock): `O(1)` per
/// datagram, the map cleared when the second changes, bounded by [`MAX_SOURCES`].
#[derive(Debug, Default)]
pub struct SourceLimit {
    second: u64,
    counts: HashMap<[u8; 8], u32>,
}

fn network(ip: IpAddr) -> [u8; 8] {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    let mut key = [0u8; 8];
    match ip {
        IpAddr::V4(v4) => {
            key[0] = 4;
            key[1..4].copy_from_slice(&v4.octets()[..3]);
        }
        IpAddr::V6(v6) => {
            key[0] = 6;
            key[1..7].copy_from_slice(&v6.octets()[..6]);
        }
    }
    key
}

impl SourceLimit {
    /// Whether `from` may get an answer in `second` (and counts it).
    pub fn allow(&mut self, from: SocketAddr, second: u64) -> bool {
        if second != self.second {
            self.second = second;
            self.counts.clear();
        }
        let key = network(from.ip());
        if !self.counts.contains_key(&key) && self.counts.len() >= MAX_SOURCES {
            return false;
        }
        let n = self.counts.entry(key).or_insert(0);
        if *n >= BINDING_PER_SECOND {
            return false;
        }
        *n += 1;
        true
    }
}

/// Answers binding requests on `socket` until `shutdown` resolves.
pub async fn serve_binding(socket: Arc<UdpSocket>, shutdown: impl Future<Output = ()>) {
    let mut buf = [0u8; READ_BUF];
    let mut limit = SourceLimit::default();
    let mut dropped: u64 = 0;
    let mut stats = tokio::time::interval(Duration::from_secs(60));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return,
            _ = stats.tick() => {
                if dropped > 0 {
                    tracing::info!(dropped, "binding requests over the per-network limit");
                    dropped = 0;
                }
            }
            read = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = read else { continue };
                let Some(answer) = wire::binding_answer(&buf[..n], from) else { continue };
                if !limit.allow(from, u64::try_from(now()).unwrap_or(0)) {
                    dropped += 1;
                    continue;
                }
                let _ = socket.send_to(&answer, from).await;
            }
        }
    }
}

/// Runs `relay` on `socket` until `shutdown` resolves; returns the final state (tests read it).
pub async fn serve_rendezvous(
    socket: Arc<UdpSocket>,
    mut relay: Rendezvous,
    shutdown: impl Future<Output = ()>,
) -> Rendezvous {
    let mut buf = [0u8; READ_BUF];
    // The rendezvous port answers binding requests too (CR-S57, MATCHMAKING_API.md §5b): a
    // launcher's NAT test compares the mapped ports of 3478 and of this port. A STUN request
    // never starts with `FIR1`, so the two never collide; the same per-network limit applies.
    let mut limit = SourceLimit::default();
    let mut sweep = tokio::time::interval(Duration::from_secs(5));
    let mut stats = tokio::time::interval(Duration::from_secs(60));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return relay,
            _ = sweep.tick() => {
                relay.sweep(now());
            }
            _ = stats.tick() => {
                let s = relay.stats();
                tracing::info!(
                    tracked = relay.tracked(),
                    relayed = relay.relayed(),
                    hellos = s.hellos,
                    ignored = s.ignored,
                    refused = s.refused,
                    forwarded = s.forwarded,
                    "relay"
                );
            }
            read = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = read else { continue };
                if let Some(answer) = wire::binding_answer(&buf[..n], from) {
                    if limit.allow(from, u64::try_from(now()).unwrap_or(0)) {
                        let _ = socket.send_to(&answer, from).await;
                    }
                    continue;
                }
                let out = relay.handle(from, &buf[..n], now());
                send_all(&socket, out).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::testing;
    use crate::rendezvous::Config;
    use crate::wire::{Message, Role, grant_id};

    #[test]
    fn binding_answers_are_limited_per_source_network_and_second() {
        let mut l = SourceLimit::default();
        let a: SocketAddr = "198.51.100.7:4000".parse().unwrap();
        let same_net: SocketAddr = "198.51.100.200:5000".parse().unwrap();
        let other: SocketAddr = "203.0.113.9:4000".parse().unwrap();
        for _ in 0..BINDING_PER_SECOND / 2 {
            assert!(l.allow(a, 10) && l.allow(same_net, 10));
        }
        assert!(!l.allow(a, 10), "one /24 shares the budget");
        assert!(l.allow(other, 10), "other networks are not affected");
        assert!(l.allow(a, 11), "the next second starts afresh");
        let mapped: SocketAddr = "[::ffff:198.51.100.7]:1".parse().unwrap();
        assert_eq!(network(mapped.ip()), network(a.ip()));
        // The map is bounded: new networks wait for the next second once it is full.
        let mut l = SourceLimit::default();
        for i in 0..MAX_SOURCES {
            let ip = std::net::Ipv4Addr::from(((i as u32) << 8) | 1);
            assert!(l.allow(SocketAddr::from((ip, 1)), 5));
        }
        assert!(!l.allow("192.0.2.1:1".parse().unwrap(), 5));
        assert!(l.allow("192.0.2.1:1".parse().unwrap(), 6));
    }

    async fn sock() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").await.unwrap()
    }

    async fn recv(s: &UdpSocket) -> Option<Vec<u8>> {
        let mut buf = [0u8; READ_BUF];
        match tokio::time::timeout(Duration::from_millis(300), s.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
            _ => None,
        }
    }

    fn hello(grant: &str, role: Role, seed: u8, cookie: Option<[u8; 16]>) -> Vec<u8> {
        let (claims, signature) =
            testing::sign(seed, &testing::claims_for(grant, role.claim(), now()));
        Message::Hello {
            role,
            claims: &claims,
            signature,
            cookie,
        }
        .encode()
    }

    /// Hello, the relay's challenge, hello with the cookie.
    async fn say_hello(s: &UdpSocket, relay: SocketAddr, grant: &str, role: Role) {
        s.send_to(&hello(grant, role, 7, None), relay)
            .await
            .unwrap();
        let challenge = recv(s).await.expect("a challenge");
        let Some(Message::Challenge { cookie, .. }) = Message::parse(&challenge) else {
            panic!("not a challenge");
        };
        s.send_to(&hello(grant, role, 7, Some(cookie)), relay)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn loopback_pairs_a_grant_ignores_unknown_ones_and_forwards() {
        let relay_sock = Arc::new(sock().await);
        let relay_addr = relay_sock.local_addr().unwrap();
        let binding_sock = Arc::new(sock().await);
        let binding_addr = binding_sock.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (stop_b_tx, stop_b_rx) = tokio::sync::oneshot::channel::<()>();
        let config = Config {
            fallback: true,
            ..Config::default()
        };
        let task = tokio::spawn(serve_rendezvous(
            relay_sock,
            Rendezvous::new(testing::keys(7), config),
            async move {
                let _ = stop_rx.await;
            },
        ));
        let binding = tokio::spawn(serve_binding(binding_sock, async move {
            let _ = stop_b_rx.await;
        }));

        let (joiner, host, stranger) = (sock().await, sock().await, sock().await);
        // Binding: the sender learns its own endpoint.
        let mut req = vec![0x00, 0x01, 0x00, 0x00];
        req.extend_from_slice(&wire::COOKIE.to_be_bytes());
        req.extend_from_slice(&[3u8; 12]);
        joiner.send_to(&req, binding_addr).await.unwrap();
        let answer = recv(&joiner).await.expect("binding answer");
        assert_eq!(
            wire::mapped_address(&answer),
            Some(joiner.local_addr().unwrap())
        );
        // CR-S57: the rendezvous port answers the same request (the NAT test's second ping), so
        // a launcher compares both mapped ports from one socket.
        joiner.send_to(&req, relay_addr).await.unwrap();
        let answer = recv(&joiner)
            .await
            .expect("binding answer on the rendezvous port");
        assert_eq!(
            wire::mapped_address(&answer),
            Some(joiner.local_addr().unwrap())
        );

        let g = "0192f0c4-0000-7000-8000-000000000001";
        let id = grant_id(g).unwrap();
        stranger
            .send_to(
                &hello(
                    "0192f0c4-0000-7000-8000-000000000002",
                    Role::Joiner,
                    9,
                    None,
                ),
                relay_addr,
            )
            .await
            .unwrap();
        say_hello(&joiner, relay_addr, g, Role::Joiner).await;
        say_hello(&host, relay_addr, g, Role::Host).await;
        let to_joiner = recv(&joiner).await.expect("peer for the joiner");
        let to_host = recv(&host).await.expect("peer for the host");
        let expect = |role, endpoint: SocketAddr| {
            Message::Peer {
                grant: id,
                role,
                endpoint,
            }
            .encode()
        };
        assert_eq!(to_joiner, expect(Role::Host, host.local_addr().unwrap()));
        assert_eq!(to_host, expect(Role::Joiner, joiner.local_addr().unwrap()));
        assert_eq!(
            recv(&stranger).await,
            None,
            "an unknown grant gets no answer"
        );

        joiner
            .send_to(&Message::Relay { grant: id }.encode(), relay_addr)
            .await
            .unwrap();
        assert!(recv(&joiner).await.is_some() && recv(&host).await.is_some());
        joiner
            .send_to(
                &Message::Data {
                    grant: id,
                    payload: b"lap",
                }
                .encode(),
                relay_addr,
            )
            .await
            .unwrap();
        assert_eq!(
            recv(&host).await,
            Some(
                Message::Data {
                    grant: id,
                    payload: b"lap"
                }
                .encode()
            )
        );

        stop_tx.send(()).unwrap();
        stop_b_tx.send(()).unwrap();
        let state = task.await.unwrap();
        binding.await.unwrap();
        assert_eq!(state.relayed(), 1);
        assert_eq!(state.stats().forwarded, 1);
    }
}
