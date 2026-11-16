//! The rendezvous and the fallback forwarder as a state machine without I/O: datagrams in,
//! datagrams out, the clock passed in (Unix seconds), so the loopback test and the VM's socket
//! loop ([`crate::udp`]) run the same code.
//!
//! - **Pairing.** A `hello` with a valid grant whose `role` claim is the side it says registers
//!   the sender's public endpoint as that grant's joiner or host, once the sender proved it
//!   receives at that address: the first `hello` is answered with a `challenge` cookie (keyed by
//!   a random per-process key, the minute and the endpoint; much smaller than the `hello`), and
//!   only a `hello` echoing it registers. A joiner's grant can never claim the host's side, and a
//!   spoofed source can never be registered. Once both sides are known, every registering
//!   `hello` (the first pairing, and the keep-alives that hold NAT bindings open) answers both
//!   sides with a `peer` naming the other side's endpoint. A datagram with an unknown, expired or
//!   forged grant is ignored: the relay never answers what it cannot verify.
//! - **Fallback.** When punching failed, either side sends `relay`. With `fallback` on and fewer
//!   than `max_peers` grants relayed, both sides get `relay ok` and the relay forwards `data`
//!   between exactly the two registered endpoints; otherwise the asker gets `relay
//!   unavailable`. The 25th peer of a 24-peer relay is refused, not queued.
//! - **Lifetime.** A grant's state lives until its `exp`; a fresher grant for the same id (the
//!   host re-presents the one `GET /mm/v1/punches` re-signs while the peer stays in the world)
//!   extends it. Everything is in memory and lost on restart: peers say `hello` again.
//! - **Bounds.** At most `max_grants` grants are tracked; expired ones are pruned first, and a new
//!   grant beyond the bound is ignored.

use crate::grant::VerifyKeys;
use crate::wire::{GrantId, Message, Role, grant_id};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};

/// How the relay behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// `FI_RELAY_FALLBACK`: forward for peers that could not punch.
    pub fallback: bool,
    /// `FI_RELAY_MAX_PEERS` (default 24): grants forwarded at once.
    pub max_peers: usize,
    /// Grants tracked at once (default 4096).
    pub max_grants: usize,
    /// This relay's own node (announce) public key (CR-S57): a grant whose signed `relay` claim
    /// names another key is ignored, and without a node key every grant naming a relay is
    /// ignored. Grants without the claim are handled as before.
    pub node_key: Option<[u8; 32]>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fallback: false,
            max_peers: 24,
            max_grants: 4096,
            node_key: None,
        }
    }
}

#[derive(Debug)]
struct Entry {
    exp: i64,
    joiner: Option<SocketAddr>,
    host: Option<SocketAddr>,
    relayed: bool,
}

impl Entry {
    fn side(&self, role: Role) -> Option<SocketAddr> {
        match role {
            Role::Joiner => self.joiner,
            Role::Host => self.host,
        }
    }

    fn role_of(&self, from: SocketAddr) -> Option<Role> {
        if self.joiner == Some(from) {
            Some(Role::Joiner)
        } else if self.host == Some(from) {
            Some(Role::Host)
        } else {
            None
        }
    }
}

/// Counters for logs and the health line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Grants verified and tracked.
    pub hellos: u64,
    /// Datagrams ignored (unknown, forged, expired, malformed, not from a registered side).
    pub ignored: u64,
    /// `relay unavailable` answers.
    pub refused: u64,
    /// Datagrams forwarded.
    pub forwarded: u64,
    /// Challenges sent (hellos without a valid cookie).
    pub challenged: u64,
}

/// The relay's state.
#[derive(Debug)]
pub struct Rendezvous {
    keys: VerifyKeys,
    config: Config,
    cookie_key: [u8; 32],
    grants: HashMap<GrantId, Entry>,
    stats: Stats,
}

/// Datagrams to send.
pub type Out = Vec<(SocketAddr, Vec<u8>)>;

impl Rendezvous {
    /// An empty relay with a fresh random cookie key.
    pub fn new(keys: VerifyKeys, config: Config) -> Self {
        let mut cookie_key = [0u8; 32];
        getrandom::fill(&mut cookie_key).expect("the system random source is unavailable");
        Self::with_cookie_key(keys, config, cookie_key)
    }

    /// An empty relay with a given cookie key (tests).
    pub fn with_cookie_key(keys: VerifyKeys, config: Config, cookie_key: [u8; 32]) -> Self {
        Self {
            keys,
            config,
            cookie_key,
            grants: HashMap::new(),
            stats: Stats::default(),
        }
    }

    /// The cookie of `from` for `grant` in `minute`.
    pub fn cookie(&self, from: SocketAddr, grant: &GrantId, minute: i64) -> [u8; 16] {
        let mut h = Sha256::new();
        h.update(self.cookie_key);
        h.update(minute.to_be_bytes());
        match from.ip() {
            IpAddr::V4(v4) => h.update(v4.octets()),
            IpAddr::V6(v6) => h.update(v6.octets()),
        }
        h.update(from.port().to_be_bytes());
        h.update(grant);
        let digest = h.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        out
    }

    fn cookie_ok(
        &self,
        from: SocketAddr,
        grant: &GrantId,
        cookie: Option<[u8; 16]>,
        now: i64,
    ) -> bool {
        let minute = now.div_euclid(60);
        cookie.is_some_and(|c| {
            [minute, minute - 1]
                .iter()
                .any(|m| ct_eq(&self.cookie(from, grant, *m), &c))
        })
    }

    /// Counters so far.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Grants forwarded now.
    pub fn relayed(&self) -> usize {
        self.grants.values().filter(|e| e.relayed).count()
    }

    /// Grants tracked now.
    pub fn tracked(&self) -> usize {
        self.grants.len()
    }

    /// Drops every grant past its `exp`.
    pub fn sweep(&mut self, now: i64) {
        self.grants.retain(|_, e| e.exp > now);
    }

    fn ignore(&mut self) -> Out {
        self.stats.ignored += 1;
        Vec::new()
    }

    /// One datagram from `from` on the rendezvous port.
    pub fn handle(&mut self, from: SocketAddr, data: &[u8], now: i64) -> Out {
        match Message::parse(data) {
            Some(Message::Hello {
                role,
                claims,
                signature,
                cookie,
            }) => self.hello(from, role, claims, &signature, cookie, now),
            Some(Message::Relay { grant }) => self.relay(from, grant, now),
            Some(Message::Data { grant, payload }) => self.data(from, grant, payload, now),
            _ => self.ignore(),
        }
    }

    fn hello(
        &mut self,
        from: SocketAddr,
        role: Role,
        claims: &[u8],
        signature: &[u8; 64],
        cookie: Option<[u8; 16]>,
        now: i64,
    ) -> Out {
        let Ok(verified) = self.keys.verify_raw(claims, signature, now) else {
            return self.ignore();
        };
        if verified.role != role.claim() {
            return self.ignore();
        }
        // CR-S57: a grant naming a relay is good only at that relay.
        if let Some(named) = &verified.relay {
            let ours = self.config.node_key.is_some_and(|own| {
                STANDARD
                    .decode(&named.key)
                    .is_ok_and(|k| k.as_slice() == own.as_slice())
            });
            if !ours {
                return self.ignore();
            }
        }
        let Some(id) = grant_id(&verified.grant) else {
            return self.ignore();
        };
        if !self.cookie_ok(from, &id, cookie, now) {
            self.stats.challenged += 1;
            let cookie = self.cookie(from, &id, now.div_euclid(60));
            return vec![(from, Message::Challenge { grant: id, cookie }.encode())];
        }
        if !self.grants.contains_key(&id) && self.grants.len() >= self.config.max_grants {
            self.sweep(now);
            if self.grants.len() >= self.config.max_grants {
                return self.ignore();
            }
        }
        self.stats.hellos += 1;
        let entry = self.grants.entry(id).or_insert(Entry {
            exp: verified.exp,
            joiner: None,
            host: None,
            relayed: false,
        });
        entry.exp = entry.exp.max(verified.exp);
        match role {
            Role::Joiner => entry.joiner = Some(from),
            Role::Host => entry.host = Some(from),
        }
        match (entry.joiner, entry.host) {
            (Some(joiner), Some(host)) => vec![
                (
                    joiner,
                    Message::Peer {
                        grant: id,
                        role: Role::Host,
                        endpoint: host,
                    }
                    .encode(),
                ),
                (
                    host,
                    Message::Peer {
                        grant: id,
                        role: Role::Joiner,
                        endpoint: joiner,
                    }
                    .encode(),
                ),
            ],
            _ => Vec::new(),
        }
    }

    fn live(&self, grant: &GrantId, from: SocketAddr, now: i64) -> Option<Role> {
        self.grants
            .get(grant)
            .filter(|e| e.exp > now)
            .and_then(|e| e.role_of(from))
    }

    fn relay(&mut self, from: SocketAddr, grant: GrantId, now: i64) -> Out {
        if self.live(&grant, from, now).is_none() {
            return self.ignore();
        }
        let relayed = self.relayed();
        let config = self.config;
        let Some(entry) = self.grants.get_mut(&grant) else {
            return self.ignore();
        };
        let (Some(joiner), Some(host)) = (entry.joiner, entry.host) else {
            // Nothing to forward to yet: the other side has not said hello.
            self.stats.refused += 1;
            return vec![(from, Message::RelayUnavailable { grant }.encode())];
        };
        if !entry.relayed {
            if !config.fallback || relayed >= config.max_peers {
                self.stats.refused += 1;
                return vec![(from, Message::RelayUnavailable { grant }.encode())];
            }
            entry.relayed = true;
        }
        let ok = Message::RelayOk { grant }.encode();
        vec![(joiner, ok.clone()), (host, ok)]
    }

    fn data(&mut self, from: SocketAddr, grant: GrantId, payload: &[u8], now: i64) -> Out {
        let Some(role) = self.live(&grant, from, now) else {
            return self.ignore();
        };
        let Some(to) = self
            .grants
            .get(&grant)
            .filter(|e| e.relayed)
            .and_then(|e| e.side(role.other()))
        else {
            return self.ignore();
        };
        self.stats.forwarded += 1;
        vec![(to, Message::Data { grant, payload }.encode())]
    }
}

fn ct_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::testing;

    const NOW: i64 = 1_792_245_600;

    fn hello(seed: u8, grant: &str, role: Role, now: i64, cookie: Option<[u8; 16]>) -> Vec<u8> {
        let (claims, signature) =
            testing::sign(seed, &testing::claims_for(grant, role.claim(), now));
        Message::Hello {
            role,
            claims: &claims,
            signature,
            cookie,
        }
        .encode()
    }

    /// Says hello, answers the challenge, returns what the registering hello produced.
    fn register(r: &mut Rendezvous, from: SocketAddr, grant: &str, role: Role, now: i64) -> Out {
        let first = r.handle(from, &hello(7, grant, role, now, None), now);
        let [(to, bytes)] = first.as_slice() else {
            panic!("one challenge expected, got {first:?}");
        };
        assert_eq!(*to, from);
        let Some(Message::Challenge { cookie, .. }) = Message::parse(bytes) else {
            panic!("not a challenge");
        };
        assert!(
            bytes.len() < hello(7, grant, role, now, None).len() / 4,
            "no amplification"
        );
        r.handle(from, &hello(7, grant, role, now, Some(cookie)), now)
    }

    /// CR-S57: a grant naming a relay is answered only by the relay whose node key it names.
    #[test]
    fn a_grant_naming_another_relay_or_any_relay_without_a_node_key_is_ignored() {
        let own = [5u8; 32];
        let named = |key: [u8; 32]| {
            let mut c = testing::claims_for(&uuid(9), "joiner", NOW);
            c.relay = Some(fi_protocol::matchmaking::RelayGrantRelay {
                key: STANDARD.encode(key),
                addr: "203.0.113.1".into(),
                binding: 3478,
                port: 5101,
            });
            let (claims, signature) = testing::sign(7, &c);
            Message::Hello {
                role: Role::Joiner,
                claims: &claims,
                signature,
                cookie: None,
            }
            .encode()
        };
        let j = addr("198.51.100.1:40001");
        let config = Config {
            node_key: Some(own),
            ..Config::default()
        };
        let mut r = Rendezvous::with_cookie_key(testing::keys(7), config, [1u8; 32]);
        assert_eq!(
            r.handle(j, &named(own), NOW).len(),
            1,
            "its own key: a challenge"
        );
        assert!(
            r.handle(j, &named([6u8; 32]), NOW).is_empty(),
            "another relay's"
        );
        let mut anonymous = relay();
        assert!(
            anonymous.handle(j, &named(own), NOW).is_empty(),
            "no node key: never a relay-claim grant"
        );
        assert_eq!(
            anonymous
                .handle(j, &hello(7, &uuid(9), Role::Joiner, NOW, None), NOW)
                .len(),
            1,
            "a grant without the claim works as before"
        );
    }

    fn uuid(i: u32) -> String {
        format!("0192f0c4-0000-7000-8000-{i:012x}")
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn relay() -> Rendezvous {
        Rendezvous::with_cookie_key(testing::keys(7), Config::default(), [1u8; 32])
    }

    #[test]
    fn pairs_two_sides_after_return_routability_and_ignores_forgeries() {
        let mut r = relay();
        let (j, h) = (addr("198.51.100.1:40001"), addr("203.0.113.5:5101"));
        assert!(register(&mut r, j, &uuid(1), Role::Joiner, NOW).is_empty());
        let out = register(&mut r, h, &uuid(1), Role::Host, NOW);
        let id = grant_id(&uuid(1)).unwrap();
        assert_eq!(
            out,
            vec![
                (
                    j,
                    Message::Peer {
                        grant: id,
                        role: Role::Host,
                        endpoint: h
                    }
                    .encode()
                ),
                (
                    h,
                    Message::Peer {
                        grant: id,
                        role: Role::Joiner,
                        endpoint: j
                    }
                    .encode()
                ),
            ]
        );
        // Forged (another key), expired, garbage: no answer at all.
        assert!(
            r.handle(j, &hello(8, &uuid(2), Role::Joiner, NOW, None), NOW)
                .is_empty()
        );
        assert!(
            r.handle(j, &hello(7, &uuid(3), Role::Joiner, NOW - 61, None), NOW)
                .is_empty()
        );
        assert!(r.handle(j, b"hello", NOW).is_empty());
        // A joiner's grant never claims the host's side.
        let (claims, signature) = testing::sign(7, &testing::claims_for(&uuid(1), "joiner", NOW));
        let as_host = Message::Hello {
            role: Role::Host,
            claims: &claims,
            signature,
            cookie: None,
        }
        .encode();
        assert!(r.handle(addr("192.0.2.9:9"), &as_host, NOW).is_empty());
        // A cookie is bound to its address: replayed from elsewhere it registers nothing.
        let cookie = r.cookie(j, &id, NOW.div_euclid(60));
        let spoof = addr("192.0.2.66:1");
        let out = r.handle(
            spoof,
            &hello(7, &uuid(1), Role::Joiner, NOW, Some(cookie)),
            NOW,
        );
        assert!(
            matches!(Message::parse(&out[0].1), Some(Message::Challenge { .. })),
            "the spoofed source only gets a challenge (which it never receives)"
        );
        assert_eq!(r.grants[&id].joiner, Some(j));
        // Relay or data for a grant never seen, or from a stranger: ignored.
        assert!(
            r.handle(spoof, &Message::Relay { grant: id }.encode(), NOW)
                .is_empty()
        );
        let other = grant_id(&uuid(9)).unwrap();
        assert!(
            r.handle(j, &Message::Relay { grant: other }.encode(), NOW)
                .is_empty()
        );
        assert_eq!(r.tracked(), 1);
        // A cookie of the previous minute still counts; two minutes old does not.
        let late = NOW + 60;
        let c = r.cookie(h, &id, NOW.div_euclid(60));
        assert!(
            !r.handle(h, &hello(7, &uuid(1), Role::Host, late, Some(c)), late)
                .iter()
                .any(|(_, b)| matches!(Message::parse(b), Some(Message::Challenge { .. })))
        );
        let too_late = NOW + 120;
        let out = r.handle(
            h,
            &hello(7, &uuid(1), Role::Host, too_late, Some(c)),
            too_late,
        );
        assert!(matches!(
            Message::parse(&out[0].1),
            Some(Message::Challenge { .. })
        ));
    }

    #[test]
    fn fallback_forwards_between_the_two_sides_only_and_is_capped() {
        let config = Config {
            fallback: true,
            max_peers: 24,
            max_grants: 4096,
            node_key: None,
        };
        let mut r = Rendezvous::with_cookie_key(testing::keys(7), config, [2u8; 32]);
        let mut pairs = Vec::new();
        for i in 0..25u32 {
            let j = addr(&format!("198.51.100.{}:4000", i + 1));
            let h = addr("203.0.113.5:5101");
            register(&mut r, j, &uuid(i), Role::Joiner, NOW);
            register(&mut r, h, &uuid(i), Role::Host, NOW);
            pairs.push((grant_id(&uuid(i)).unwrap(), j, h));
        }
        for (n, (id, j, h)) in pairs.iter().enumerate() {
            let out = r.handle(*j, &Message::Relay { grant: *id }.encode(), NOW);
            if n < 24 {
                let ok = Message::RelayOk { grant: *id }.encode();
                assert_eq!(out, vec![(*j, ok.clone()), (*h, ok)], "peer {}", n + 1);
            } else {
                assert_eq!(
                    out,
                    vec![(*j, Message::RelayUnavailable { grant: *id }.encode())],
                    "the 25th peer is refused"
                );
            }
        }
        assert_eq!(r.relayed(), 24);
        let (id, j, h) = pairs[0];
        let data = |payload: &'static [u8]| Message::Data { grant: id, payload }.encode();
        assert_eq!(r.handle(j, &data(b"lap"), NOW), vec![(h, data(b"lap"))]);
        assert_eq!(r.handle(h, &data(b"ack"), NOW)[0].0, j);
        assert!(
            r.handle(addr("192.0.2.1:9"), &data(b"x"), NOW).is_empty(),
            "only the registered endpoints are forwarded"
        );
        let (id25, j25, _) = pairs[24];
        assert!(
            r.handle(
                j25,
                &Message::Data {
                    grant: id25,
                    payload: b"x"
                }
                .encode(),
                NOW
            )
            .is_empty(),
            "a refused grant forwards nothing"
        );
        r.sweep(NOW + 60);
        assert_eq!(r.tracked(), 0);
    }

    #[test]
    fn without_fallback_relay_is_refused_and_a_fresher_grant_extends_life() {
        let mut r = relay();
        let (j, h) = (addr("198.51.100.1:1"), addr("203.0.113.5:2"));
        register(&mut r, j, &uuid(1), Role::Joiner, NOW);
        let id = grant_id(&uuid(1)).unwrap();
        assert_eq!(
            r.handle(j, &Message::Relay { grant: id }.encode(), NOW),
            vec![(j, Message::RelayUnavailable { grant: id }.encode())],
            "the other side has not said hello"
        );
        register(&mut r, h, &uuid(1), Role::Host, NOW);
        assert_eq!(
            r.handle(j, &Message::Relay { grant: id }.encode(), NOW),
            vec![(j, Message::RelayUnavailable { grant: id }.encode())],
            "fallback is off"
        );
        register(&mut r, h, &uuid(1), Role::Host, NOW + 50);
        r.sweep(NOW + 70);
        assert_eq!(r.tracked(), 1, "re-presented by the host, still alive");
        r.sweep(NOW + 110);
        assert_eq!(r.tracked(), 0);
    }

    #[test]
    fn the_grant_table_is_bounded() {
        let mut r = Rendezvous::with_cookie_key(
            testing::keys(7),
            Config {
                fallback: false,
                max_peers: 1,
                max_grants: 2,
                node_key: None,
            },
            [3u8; 32],
        );
        let j = addr("198.51.100.1:1");
        for i in 0..2 {
            register(&mut r, j, &uuid(i), Role::Joiner, NOW);
        }
        assert_eq!(r.tracked(), 2);
        let first = r.handle(j, &hello(7, &uuid(2), Role::Joiner, NOW, None), NOW);
        let Some(Message::Challenge { cookie, .. }) = Message::parse(&first[0].1) else {
            panic!()
        };
        assert!(
            r.handle(j, &hello(7, &uuid(2), Role::Joiner, NOW, Some(cookie)), NOW)
                .is_empty()
        );
        assert_eq!(
            r.tracked(),
            2,
            "the third grant is ignored while two are live"
        );
        register(&mut r, j, &uuid(5), Role::Joiner, NOW + 61);
        assert_eq!(r.tracked(), 1, "expired ones are pruned to make room");
    }
}
