// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! A private world's view of its own NAT (contract §5.1), driven by the world loop on the world
//! socket: STUN rounds to the configured servers (RFC 8489 Binding) that tell the public mapping
//! and whether it is the same for every server (cone) or not (symmetric), a keepalive to one IPv4
//! server between rounds so short router timeouts do not move the announced port, Binding answers
//! to the joiners' probes (under a global and a per-address budget, so the socket is no useful
//! reflector), bounded punches towards peers, and the facts the controller reads (`NetworkFacts`).
//! Name lookups and the UPnP mapping (`crate::upnp`) run on their own threads and post back
//! through channels the world's network tick drains; the loop itself never blocks.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::{stun, upnp};

/// The servers asked when the controller names none.
pub const STUN_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// One IPv4 server is asked this often between rounds (0: never).
pub const KEEPALIVE_MS: u64 = 5_000;

/// Time between full rounds unless configured.
const ROUND_EVERY_MS: u64 = 25_000;
/// A request without an answer after this long is forgotten.
const REQUEST_LIFETIME_MS: u64 = 10_000;
/// An observation is kept at least this long (and at least three rounds).
const OBSERVATION_MIN_MS: u64 = 60_000;
/// A changed mapping triggers a new round only if the last one is at least this old.
const REMEASURE_GAP_MS: u64 = 1_000;
const REQUESTS_IN_FLIGHT: usize = 16;
const SERVERS_KEPT: usize = 16;
const NAMES_RESOLVED: usize = 8;
/// A punch is a Binding request every 200 ms, ten times, to at most 32 endpoints at once.
const PUNCH_REPEATS: u8 = 10;
const PUNCH_SPACING_MS: u64 = 200;
const PUNCH_TARGETS: usize = 32;
/// How often a live UPnP thread's channel is looked at once the first outcome is in.
const MAPPING_CHECK_MS: u64 = 1_000;

/// Binding answers per second, all sources together.
const ANSWERS_PER_WINDOW: u32 = 50;
/// Binding answers per second to one IP address. A joining hook probes each of its (at most 8)
/// candidates every 250 ms (`proxy.c`), and two or three of them (STUN, UPnP and host addresses)
/// can land on this socket: about 12 per second per joiner, so 24 covers two joiners behind one
/// NAT; at 44 bytes an answer that is about 1 KiB/s towards a spoofed victim at most.
const ANSWERS_PER_ADDRESS: u32 = 24;
const ANSWER_WINDOW_MS: u64 = 1_000;

/// What a private world's NAT handling is told by `world.open`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NatSettings {
    /// `host:port` names, resolved off the loop when `discovery` is on.
    pub stun_servers: Vec<String>,
    pub stun_every_ms: u64,
    pub keepalive_ms: u64,
    /// Resolve the STUN servers (a world without it measures only servers set directly).
    pub discovery: bool,
    /// Map the port on the router.
    pub upnp: bool,
    /// The world socket's port (the UPnP internal port).
    pub port: u16,
}

impl NatSettings {
    pub fn new(port: u16, discovery: bool, upnp: bool) -> NatSettings {
        Self {
            stun_servers: STUN_SERVERS.iter().map(|s| (*s).to_string()).collect(),
            stun_every_ms: ROUND_EVERY_MS,
            keepalive_ms: KEEPALIVE_MS,
            discovery,
            upnp,
            port,
        }
    }
}

/// The NAT kind as two IPv4 servers see the world socket.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NatKind {
    #[default]
    Unknown,
    Cone,
    Symmetric,
}

impl NatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Cone => "cone",
            Self::Symmetric => "symmetric",
        }
    }
}

/// Where the UPnP mapping stands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Upnp {
    #[default]
    Off,
    Pending,
    NotFound,
    Mapped,
    Failed,
}

impl Upnp {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pending => "pending",
            Self::NotFound => "not_found",
            Self::Mapped => "mapped",
            Self::Failed => "failed",
        }
    }
}

/// What changed off the loop since the last [`Nat::take_news`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NatNews {
    /// The STUN names resolved to these servers (already applied).
    Servers(Vec<SocketAddr>),
    /// A UPnP attempt ended (already applied).
    Upnp(crate::upnp::Outcome),
}

/// The facts `NetworkFacts` and `diag` report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NatFacts {
    pub kind: NatKind,
    /// The first public endpoint seen per address family.
    pub public: Vec<SocketAddr>,
    pub upnp: Upnp,
    pub upnp_address: Option<SocketAddr>,
    /// The router's external address is itself private or shared (double NAT, CGNAT).
    pub behind_another_nat: bool,
}

/// A request to a server awaiting its answer.
struct Asked {
    server: SocketAddr,
    txid: stun::TxId,
    at: u64,
}

/// What one server last said the world socket looks like from outside.
struct Seen {
    server: SocketAddr,
    public: SocketAddr,
    at: u64,
}

struct PunchTarget {
    to: SocketAddr,
    txid: stun::TxId,
    sends_left: u8,
    due: u64,
}

/// The per-second Binding answer allowance: a fixed one-second window with a total and a count
/// per source address key (`net::ip_key`: an IPv6 source counts as its /64; at most [`ANSWERS_PER_WINDOW`] entries, since every entry took an answer).
struct AnswerBudget {
    window: u64,
    total: u32,
    by_address: Vec<(IpAddr, u32)>,
}

impl AnswerBudget {
    fn new(now: u64) -> AnswerBudget {
        AnswerBudget {
            window: now,
            total: 0,
            by_address: Vec::new(),
        }
    }

    fn allow(&mut self, ip: IpAddr, now: u64) -> bool {
        if now.saturating_sub(self.window) >= ANSWER_WINDOW_MS {
            *self = AnswerBudget::new(now);
        }
        if self.total >= ANSWERS_PER_WINDOW {
            return false;
        }
        match self.by_address.iter_mut().find(|(a, _)| *a == ip) {
            Some((_, n)) if *n >= ANSWERS_PER_ADDRESS => return false,
            Some((_, n)) => *n += 1,
            None => self.by_address.push((ip, 1)),
        }
        self.total += 1;
        true
    }
}

/// An address a datagram may be sent to at all: a real port, not unspecified, not multicast or
/// broadcast.
fn reachable(a: SocketAddr) -> bool {
    a.port() != 0
        && match a.ip() {
            IpAddr::V4(v4) => !v4.is_unspecified() && v4.octets()[0] < 224,
            IpAddr::V6(v6) => !v6.is_unspecified() && !v6.is_multicast(),
        }
}

/// The first IPv4 and the first IPv6 address of every name, in order.
fn resolve(names: &[String]) -> Vec<SocketAddr> {
    let mut found = Vec::new();
    for name in names.iter().take(NAMES_RESOLVED) {
        let Ok(all) = name.as_str().to_socket_addrs() else {
            continue;
        };
        let all: Vec<SocketAddr> = all.filter(|a| a.port() != 0).collect();
        let v4 = all.iter().find(|a| a.is_ipv4());
        let v6 = all.iter().find(|a| a.is_ipv6());
        found.extend(v4.into_iter().chain(v6).copied());
    }
    found
}

/// One world's NAT state; see the module comment.
pub struct Nat {
    config: NatSettings,
    dual: bool,
    stun_targets: Vec<SocketAddr>,
    round_at: Option<u64>,
    last_keepalive: Option<u64>,
    keepalive_turn: usize,
    asked: Vec<Asked>,
    sightings: Vec<Seen>,
    punch_list: Vec<PunchTarget>,
    budget: AnswerBudget,
    upnp: Upnp,
    upnp_at: Option<SocketAddr>,
    pending_news: Vec<NatNews>,
    lookup: Option<Receiver<Vec<SocketAddr>>>,
    mapping: Option<Receiver<upnp::Outcome>>,
    last_tick: u64,
    stop: Arc<AtomicBool>,
}

impl Drop for Nat {
    fn drop(&mut self) {
        // the UPnP thread sees it within a quarter second and removes its mapping
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Nat {
    /// Starts the lookup thread (`discovery`) and the UPnP thread (`upnp`) as configured.
    pub fn new(settings: NatSettings, socket_is_v6: bool, now_ms: u64) -> Nat {
        let mut me = Self {
            dual: socket_is_v6,
            stun_targets: Vec::new(),
            round_at: None,
            last_keepalive: None,
            keepalive_turn: 0,
            asked: Vec::new(),
            sightings: Vec::new(),
            punch_list: Vec::new(),
            budget: AnswerBudget::new(now_ms),
            upnp: Upnp::Off,
            upnp_at: None,
            pending_news: Vec::new(),
            lookup: None,
            mapping: None,
            last_tick: now_ms,
            stop: Arc::new(AtomicBool::new(false)),
            config: settings,
        };
        if me.config.discovery {
            me.start_lookup();
        }
        if me.config.upnp {
            me.upnp = Upnp::Pending;
            me.start_mapping();
        }
        me
    }

    fn start_lookup(&mut self) {
        let (post, inbox) = mpsc::channel();
        let names = self.config.stun_servers.clone();
        let spawned = std::thread::Builder::new()
            .name("ds-stun-lookup".into())
            .spawn(move || {
                let _ = post.send(resolve(&names));
            });
        if spawned.is_ok() {
            self.lookup = Some(inbox);
        }
    }

    fn start_mapping(&mut self) {
        let (post, inbox) = mpsc::channel();
        let port = self.config.port;
        let stop = Arc::clone(&self.stop);
        let spawned = std::thread::Builder::new()
            .name("ds-upnp".into())
            .spawn(move || {
                upnp::run_while(
                    port,
                    move |o| {
                        let _ = post.send(o);
                    },
                    move || !stop.load(Ordering::Relaxed),
                );
            });
        match spawned {
            Ok(_) => self.mapping = Some(inbox),
            Err(_) => self.upnp = Upnp::Failed,
        }
    }

    /// The threads' results, applied and queued as news.
    fn collect(&mut self) {
        if let Some(rx) = &self.lookup {
            match rx.try_recv() {
                Ok(found) => {
                    self.lookup = None;
                    self.set_servers(&found);
                    self.pending_news
                        .push(NatNews::Servers(self.stun_targets.clone()));
                }
                Err(TryRecvError::Disconnected) => self.lookup = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        let mut outcomes = Vec::new();
        if let Some(rx) = &self.mapping {
            loop {
                match rx.try_recv() {
                    Ok(o) => outcomes.push(o),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.mapping = None;
                        if self.upnp == Upnp::Pending {
                            outcomes.push(upnp::Outcome::Failed);
                        }
                        break;
                    }
                }
            }
        }
        for o in outcomes {
            self.apply_mapping(o);
        }
    }

    fn apply_mapping(&mut self, o: upnp::Outcome) {
        (self.upnp, self.upnp_at) = match &o {
            upnp::Outcome::Mapped {
                external_ip: ip,
                external_port: port,
                ..
            } => (Upnp::Mapped, Some(SocketAddr::new(*ip, *port))),
            upnp::Outcome::NotFound => (Upnp::NotFound, None),
            upnp::Outcome::Failed => (Upnp::Failed, None),
        };
        self.pending_news.push(NatNews::Upnp(o));
    }

    /// A datagram from the world socket. `true` when it was STUN (handled here or not: the side
    /// channels are STUN-framed and still go on to the engine); Binding requests are answered,
    /// Binding successes complete a server request or end a punch.
    pub fn on_packet(
        &mut self,
        send: &mut dyn FnMut(SocketAddr, Vec<u8>),
        data: &[u8],
        from: SocketAddr,
        now_ms: u64,
    ) -> bool {
        let framed = stun::is_stun(data);
        let source = stun::canonical(from);
        if let Some(message) = stun::parse(data).filter(|_| framed) {
            if message.kind == stun::BINDING_REQUEST {
                if reachable(source) && self.budget.allow(crate::net::ip_key(source), now_ms) {
                    send(source, stun::binding_success(&message.txid, source));
                }
            } else if message.kind == stun::BINDING_SUCCESS {
                self.on_answer(&message, source, now_ms);
            }
        }
        framed
    }

    fn on_answer(&mut self, message: &stun::Message, from: SocketAddr, now: u64) {
        let ours = self.asked.iter().position(|q| {
            q.server == from
                && q.txid == message.txid
                && now.saturating_sub(q.at) < REQUEST_LIFETIME_MS
        });
        let Some(i) = ours else {
            self.punch_list
                .retain(|p| p.to != from || p.txid != message.txid);
            return;
        };
        self.asked.remove(i);
        let Some(public) = message
            .mapped
            .map(stun::canonical)
            .filter(|m| m.port() != 0)
        else {
            return;
        };
        let moved = self
            .sightings
            .iter()
            .any(|s| s.server == from && s.public != public);
        if moved
            && self
                .round_at
                .is_none_or(|r| now.saturating_sub(r) >= REMEASURE_GAP_MS)
        {
            // measure every server again now, so the kind never mixes two mappings
            self.round_at = None;
        }
        self.sightings.retain(|s| s.server != from);
        self.sightings.push(Seen {
            server: from,
            public,
            at: now,
        });
    }

    /// Whether a result from a thread is awaited soon (the world then polls its tick).
    pub fn polling(&self) -> bool {
        self.lookup.is_some() || (self.mapping.is_some() && self.upnp == Upnp::Pending)
    }

    fn round_every(&self) -> u64 {
        self.config.stun_every_ms.max(1)
    }

    fn has_ipv4_server(&self) -> bool {
        self.stun_targets.iter().any(SocketAddr::is_ipv4)
    }

    /// The next instant [`Nat::tick`] has work: a round, a keepalive, a punch, or a look at the
    /// UPnP thread; `None`: nothing until a datagram or a command.
    pub fn next_deadline(&self) -> Option<u64> {
        let mut times: Vec<u64> = Vec::new();
        if !self.stun_targets.is_empty() {
            times.push(self.round_at.map_or(0, |r| r + self.round_every()));
            if self.config.keepalive_ms > 0
                && self.has_ipv4_server()
                && let Some(k) = self.last_keepalive
            {
                times.push(k + self.config.keepalive_ms);
            }
        }
        times.extend(self.punch_list.iter().map(|p| p.due));
        if self.mapping.is_some() {
            times.push(self.last_tick + MAPPING_CHECK_MS);
        }
        times.into_iter().min()
    }

    /// The STUN servers to measure with (at most 16, IPv6 ones only on a dual-stack socket,
    /// canonical, no repeats).
    pub fn set_servers(&mut self, servers: &[SocketAddr]) {
        let mut kept: Vec<SocketAddr> = Vec::new();
        for s in servers
            .iter()
            .take(SERVERS_KEPT)
            .map(|s| stun::canonical(*s))
        {
            if (s.is_ipv4() || self.dual) && !kept.contains(&s) {
                kept.push(s);
            }
        }
        self.stun_targets = kept;
    }

    fn ask(&mut self, server: SocketAddr, now: u64, send: &mut dyn FnMut(SocketAddr, Vec<u8>)) {
        let txid = stun::new_txid();
        self.asked.push(Asked {
            server,
            txid,
            at: now,
        });
        send(server, stun::binding_request(&txid).to_vec());
    }

    /// Thread results, expiries, the round or the keepalive when due, and due punches.
    pub fn tick(&mut self, send: &mut dyn FnMut(SocketAddr, Vec<u8>), now_ms: u64) {
        let now = now_ms;
        self.last_tick = now;
        self.collect();
        self.asked
            .retain(|q| now.saturating_sub(q.at) < REQUEST_LIFETIME_MS);
        let keep_seen = OBSERVATION_MIN_MS.max(self.round_every().saturating_mul(3));
        self.sightings
            .retain(|s| now.saturating_sub(s.at) < keep_seen);

        let round_due = self
            .round_at
            .is_none_or(|r| now.saturating_sub(r) >= self.round_every());
        if !self.stun_targets.is_empty() && round_due {
            self.round_at = Some(now);
            self.last_keepalive = Some(now);
            for server in self.stun_targets.clone() {
                if self.asked.len() >= REQUESTS_IN_FLIGHT {
                    break;
                }
                self.ask(server, now, send);
            }
        } else if self.config.keepalive_ms > 0
            && self.asked.len() < REQUESTS_IN_FLIGHT
            && self
                .last_keepalive
                .is_none_or(|k| now.saturating_sub(k) >= self.config.keepalive_ms)
        {
            let v4: Vec<SocketAddr> = self
                .stun_targets
                .iter()
                .copied()
                .filter(SocketAddr::is_ipv4)
                .collect();
            if !v4.is_empty() {
                self.last_keepalive = Some(now);
                self.keepalive_turn = (self.keepalive_turn + 1) % v4.len();
                self.ask(v4[self.keepalive_turn], now, send);
            }
        }

        for p in self.punch_list.iter_mut().filter(|p| p.due <= now) {
            send(p.to, stun::binding_request(&p.txid).to_vec());
            p.sends_left -= 1;
            p.due = now + PUNCH_SPACING_MS;
        }
        self.punch_list.retain(|p| p.sends_left > 0);
    }

    /// Opens the NAT towards `to`: ten Binding requests 200 ms apart from now; asking again for
    /// the same endpoint restarts its count; the oldest of 32 targets gives way.
    pub fn punch(&mut self, to: SocketAddr, now_ms: u64) {
        let target = stun::canonical(to);
        if !reachable(target) || (target.is_ipv6() && !self.dual) {
            return;
        }
        if let Some(p) = self.punch_list.iter_mut().find(|p| p.to == target) {
            p.sends_left = PUNCH_REPEATS;
            p.due = now_ms;
            return;
        }
        if self.punch_list.len() >= PUNCH_TARGETS {
            let _evicted = self.punch_list.remove(0);
        }
        self.punch_list.push(PunchTarget {
            to: target,
            txid: stun::new_txid(),
            sends_left: PUNCH_REPEATS,
            due: now_ms,
        });
    }

    /// Cone when the first two IPv4 servers saw the same endpoint, symmetric when not.
    pub fn kind(&self) -> NatKind {
        let mut v4 = self
            .sightings
            .iter()
            .filter(|s| s.server.is_ipv4())
            .map(|s| s.public);
        let (Some(first), Some(second)) = (v4.next(), v4.next()) else {
            return NatKind::Unknown;
        };
        if first == second {
            NatKind::Cone
        } else {
            NatKind::Symmetric
        }
    }

    /// The first public endpoint seen in each address family.
    pub fn public(&self) -> Vec<SocketAddr> {
        let mut firsts: Vec<SocketAddr> = Vec::with_capacity(2);
        for s in &self.sightings {
            if !firsts.iter().any(|o| o.is_ipv4() == s.public.is_ipv4()) {
                firsts.push(s.public);
            }
        }
        firsts
    }

    pub fn facts(&self) -> NatFacts {
        let (kind, public) = (self.kind(), self.public());
        NatFacts {
            kind,
            public,
            upnp: self.upnp,
            upnp_address: self.upnp_at,
            behind_another_nat: self
                .upnp_at
                .is_some_and(|a| upnp::behind_another_nat(a.ip())),
        }
    }

    pub fn take_news(&mut self) -> Vec<NatNews> {
        std::mem::take(&mut self.pending_news)
    }

    /// The router's external address while mapped.
    pub fn upnp_address(&self) -> Option<IpAddr> {
        self.upnp_at.map(|a| a.ip())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(dual: bool) -> Nat {
        Nat::new(NatSettings::new(5101, false, false), dual, 0)
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn collect_sends(n: &mut Nat, now: u64) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut got = Vec::new();
        n.tick(&mut |to, d| got.push((to, d)), now);
        got
    }

    fn deliver(n: &mut Nat, data: &[u8], from: SocketAddr, now: u64) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut got = Vec::new();
        n.on_packet(&mut |to, d| got.push((to, d)), data, from, now);
        got
    }

    /// Answers every request of a round as servers seeing `public[i]` would.
    fn answer_round(n: &mut Nat, sent: &[(SocketAddr, Vec<u8>)], public: &[&str], now: u64) {
        for ((server, req), seen) in sent.iter().zip(public) {
            let txid = stun::parse(req).unwrap().txid;
            deliver(n, &stun::binding_success(&txid, addr(seen)), *server, now);
        }
    }

    #[test]
    fn the_answer_budget_caps_one_address_and_the_whole_socket() {
        let mut n = quiet(false);
        let req = stun::binding_request(&stun::new_txid());
        let victim = addr("198.51.100.20:4000");
        let mut answers = 0;
        for i in 0..100u64 {
            answers += deliver(&mut n, &req, victim, 1000 + i).len();
        }
        assert_eq!(
            answers as u32, ANSWERS_PER_ADDRESS,
            "one address, one second"
        );
        // many addresses share the global allowance (the victim's 10 already count)
        let mut others = 0;
        for i in 0..200u16 {
            let from = SocketAddr::new(IpAddr::V4([203, 0, 113, (i % 250) as u8].into()), 1000 + i);
            others += deliver(&mut n, &req, from, 1500).len();
        }
        assert_eq!(others as u32 + ANSWERS_PER_ADDRESS, ANSWERS_PER_WINDOW);
        // the next window refills both
        let a = deliver(&mut n, &req, victim, 2000);
        assert_eq!(a.len(), 1);
        let answer = stun::parse(&a[0].1).unwrap();
        assert_eq!(
            (answer.kind, answer.mapped),
            (stun::BINDING_SUCCESS, Some(victim))
        );
    }

    #[test]
    fn an_ipv6_slash_64_shares_one_answer_budget() {
        let mut b = AnswerBudget::new(0);
        let granted = (0..200u16)
            .filter(|i| {
                let ip: std::net::Ipv6Addr = format!("2001:db8:1:2::{:x}", i + 1).parse().unwrap();
                let from = SocketAddr::new(IpAddr::V6(ip), 3074);
                b.allow(crate::net::ip_key(from), 10)
            })
            .count();
        assert_eq!(granted, ANSWERS_PER_ADDRESS as usize);
    }

    #[test]
    fn unreachable_sources_get_no_answer_and_take_no_budget() {
        let mut n = quiet(true);
        let req = stun::binding_request(&stun::new_txid());
        for from in [
            "0.0.0.0:5",
            "192.0.2.1:0",
            "224.0.0.1:5",
            "255.255.255.255:5",
            "[ff02::1]:5",
            "[::]:5",
        ] {
            assert!(deliver(&mut n, &req, addr(from), 0).is_empty(), "{from}");
        }
        assert_eq!(n.budget.total, 0);
        // a v4-mapped source is answered at its IPv4 form
        let a = deliver(&mut n, &req, addr("[::ffff:192.0.2.9]:77"), 0);
        assert_eq!(a[0].0, addr("192.0.2.9:77"));
    }

    #[test]
    fn non_binding_stun_passes_through_untouched() {
        let mut n = quiet(false);
        let side = stun::indication(0x3e91, &stun::new_txid(), &[(0xc0f1, &[0u8; 16][..])]);
        let mut sent = 0;
        assert!(n.on_packet(&mut |_, _| sent += 1, &side, addr("192.0.2.1:9"), 0));
        assert!(!n.on_packet(&mut |_, _| sent += 1, b"not stun", addr("192.0.2.1:9"), 0));
        assert_eq!(sent, 0);
    }

    #[test]
    fn kind_and_public_endpoints_from_two_servers() {
        let mut n = quiet(true);
        n.set_servers(&[
            addr("192.0.2.1:3478"),
            addr("192.0.2.2:3478"),
            addr("[2001:db8::1]:3478"),
        ]);
        let sent = collect_sends(&mut n, 0);
        assert_eq!(sent.len(), 3);
        assert_eq!(n.kind(), NatKind::Unknown);
        answer_round(
            &mut n,
            &sent,
            &[
                "203.0.113.1:6000",
                "203.0.113.1:6000",
                "[2001:db8::99]:6000",
            ],
            10,
        );
        assert_eq!(n.kind(), NatKind::Cone);
        assert_eq!(
            n.public(),
            vec![addr("203.0.113.1:6000"), addr("[2001:db8::99]:6000")]
        );
        // a forged answer (wrong transaction or wrong source) changes nothing
        deliver(
            &mut n,
            &stun::binding_success(&[7; 12], addr("1.2.3.4:5")),
            addr("192.0.2.1:3478"),
            20,
        );
        let txid = stun::parse(&sent[0].1).unwrap().txid;
        deliver(
            &mut n,
            &stun::binding_success(&txid, addr("1.2.3.4:5")),
            addr("192.0.2.9:3478"),
            20,
        );
        assert_eq!(n.kind(), NatKind::Cone);
    }

    #[test]
    fn a_moved_mapping_measures_again_and_can_turn_symmetric() {
        let mut n = quiet(false);
        n.set_servers(&[addr("192.0.2.1:3478"), addr("192.0.2.2:3478")]);
        let first = collect_sends(&mut n, 0);
        answer_round(&mut n, &first, &["203.0.113.1:6000", "203.0.113.1:6000"], 5);
        let keep = collect_sends(&mut n, 5000);
        assert_eq!(keep.len(), 1);
        answer_round(&mut n, &keep, &["203.0.113.1:6001"], 5005);
        // the changed answer forces a full round on the next tick
        assert_eq!(n.next_deadline(), Some(0));
        let again = collect_sends(&mut n, 5010);
        assert_eq!(again.len(), 2);
        answer_round(
            &mut n,
            &again,
            &["203.0.113.1:6002", "203.0.113.1:6003"],
            5020,
        );
        assert_eq!(n.kind(), NatKind::Symmetric);
        // observations age out
        collect_sends(&mut n, 5020 + OBSERVATION_MIN_MS + 80_000);
        assert!(n.public().is_empty());
    }

    #[test]
    fn servers_are_filtered_and_unanswered_requests_expire() {
        let mut n = quiet(false);
        n.config.keepalive_ms = 0;
        let many: Vec<SocketAddr> = (1..=20u8)
            .map(|i| SocketAddr::new(IpAddr::V4([192, 0, 2, i].into()), 3478))
            .collect();
        let mut list = vec![addr("[::ffff:192.0.2.1]:3478"), addr("[2001:db8::1]:3478")];
        list.extend(&many);
        n.set_servers(&list);
        assert_eq!(
            n.stun_targets.len(),
            14,
            "16 taken, the IPv6 one dropped, the mapped one repeated"
        );
        assert_eq!(collect_sends(&mut n, 0).len(), 14);
        assert_eq!(n.asked.len(), 14);
        collect_sends(&mut n, REQUEST_LIFETIME_MS);
        assert_eq!(n.asked.len(), 0);
    }

    #[test]
    fn punches_repeat_then_stop_or_end_on_an_answer() {
        let mut n = quiet(false);
        n.punch(addr("[2001:db8::5]:7000"), 0);
        n.punch(addr("239.1.1.1:7000"), 0);
        assert!(n.punch_list.is_empty());
        let peer = addr("198.51.100.7:7000");
        n.punch(peer, 100);
        assert_eq!(n.next_deadline(), Some(100));
        let mut count = 0;
        for t in (100..3000).step_by(100) {
            count += collect_sends(&mut n, t)
                .iter()
                .filter(|s| s.0 == peer)
                .count();
        }
        assert_eq!(count, usize::from(PUNCH_REPEATS));
        assert!(n.punch_list.is_empty());
        n.punch(peer, 5000);
        let first = collect_sends(&mut n, 5000);
        let txid = stun::parse(&first[0].1).unwrap().txid;
        deliver(
            &mut n,
            &stun::binding_success(&txid, addr("192.0.2.50:5101")),
            peer,
            5050,
        );
        assert!(n.punch_list.is_empty());
        for i in 0..40u8 {
            n.punch(
                SocketAddr::new(IpAddr::V4([198, 51, 100, i + 1].into()), 1),
                6000,
            );
        }
        assert_eq!(n.punch_list.len(), PUNCH_TARGETS);
        assert_eq!(
            n.punch_list[0].to.ip(),
            IpAddr::V4([198, 51, 100, 9].into())
        );
    }

    #[test]
    fn upnp_outcomes_become_facts_and_news() {
        let mut n = quiet(false);
        assert_eq!(n.facts().upnp, Upnp::Off);
        let (post, inbox) = mpsc::channel();
        n.mapping = Some(inbox);
        n.upnp = Upnp::Pending;
        assert!(n.polling());
        post.send(upnp::Outcome::Mapped {
            external_ip: "100.64.1.2".parse().unwrap(),
            external_port: 5102,
            lease: upnp::LEASE_SECS,
        })
        .unwrap();
        collect_sends(&mut n, 40);
        let f = n.facts();
        assert_eq!(
            (f.upnp, f.upnp_address),
            (Upnp::Mapped, Some(addr("100.64.1.2:5102")))
        );
        assert!(f.behind_another_nat);
        assert_eq!(n.upnp_address(), Some("100.64.1.2".parse().unwrap()));
        assert!(!n.polling());
        assert_eq!(n.next_deadline(), Some(40 + MAPPING_CHECK_MS));
        assert!(matches!(
            n.take_news()[..],
            [NatNews::Upnp(upnp::Outcome::Mapped { .. })]
        ));
        drop(post);
        collect_sends(&mut n, 50);
        assert_eq!(
            n.facts().upnp,
            Upnp::Mapped,
            "a finished thread keeps the last outcome"
        );
        assert_eq!(n.next_deadline(), None);
        for (u, s) in [
            (Upnp::Off, "off"),
            (Upnp::Pending, "pending"),
            (Upnp::NotFound, "not_found"),
            (Upnp::Mapped, "mapped"),
            (Upnp::Failed, "failed"),
        ] {
            assert_eq!(u.as_str(), s);
        }
        assert_eq!(NatKind::Symmetric.as_str(), "symmetric");
    }
}
