// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! What the host's network looks like from the Internet, and the punches (contract §5.1, private
//! worlds only): STUN rounds on the world socket (stun.rs), the router's port mapping (upnp.rs,
//! its own thread) and the joining players' punches. The world owns one Nat and feeds it every
//! STUN packet and its timer; what it learns is read with `facts()` (the `world.network`
//! event's NetworkFacts) and `take_news()`. Nothing here writes a file or a log line: addresses
//! never reach stdout (contract §2.3).
//!
//! Sans-I/O: every send goes through the caller's `send` closure, every time is the caller's
//! monotonic milliseconds. Only the STUN servers' name lookup and UPnP run in threads (off the
//! loop), and post their results back through channels.
//!
//! Nothing here sends anything but STUN Binding requests (to the STUN servers and, for a punch,
//! to the player) and Binding successes (to whoever sent us a request).
//!
//! Between full STUN rounds one IPv4 server per `keepalive_ms` (5 s) refreshes the mapping
//! (`W/RelayNetworkNat.cs`): a router whose UDP idle timeout is shorter than the round would
//! otherwise hand the world a new external port while it waits for players, leaving the
//! announced one stale.

use std::{
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::mpsc::{self, Receiver},
};

use crate::{
    stun::{self, TxId},
    upnp,
};

/// Public STUN servers (no server of ours). Two, for the NAT's kind (`W/RelayNetwork.cs:23`).
pub const STUN_SERVERS: [&str; 2] = ["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// A punch: this many Binding requests, this far apart (ms).
const PUNCH_SENDS: u8 = 10;
const PUNCH_GAP_MS: u64 = 200;
const PUNCHES_MAX: usize = 32;
const PENDING_MAX: usize = 16;
const PENDING_TTL_MS: u64 = 10_000;
/// Binding requests answered per second (a flood of them gets no answers).
const ANSWERS_PER_SECOND: u32 = 50;
/// One IPv4 STUN server this often between rounds keeps the NAT's mapping (0: off).
pub const KEEPALIVE_MS: u64 = 5_000;
/// At most this many STUN servers are used.
const SERVERS_MAX: usize = 16;

#[derive(Debug, Clone)]
pub struct NatSettings {
    pub stun_servers: Vec<String>,
    pub stun_every_ms: u64,
    /// The keepalive between rounds (0: none).
    pub keepalive_ms: u64,
    pub discovery: bool,
    pub upnp: bool,
    pub port: u16,
}

impl NatSettings {
    pub fn new(port: u16, discovery: bool, upnp: bool) -> NatSettings {
        NatSettings {
            stun_servers: STUN_SERVERS.iter().map(|s| s.to_string()).collect(),
            stun_every_ms: 25_000,
            keepalive_ms: KEEPALIVE_MS,
            discovery,
            upnp,
            port,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatKind {
    Unknown,
    Cone,
    Symmetric,
}

impl NatKind {
    pub fn as_str(self) -> &'static str {
        match self {
            NatKind::Unknown => "unknown",
            NatKind::Cone => "cone",
            NatKind::Symmetric => "symmetric",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Upnp {
    Off,
    Pending,
    Done(upnp::Outcome),
}

impl Upnp {
    /// NetworkFacts' `upnp`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Upnp::Off => "off",
            Upnp::Pending => "pending",
            Upnp::Done(upnp::Outcome::NotFound) => "not_found",
            Upnp::Done(upnp::Outcome::Mapped { .. }) => "mapped",
            Upnp::Done(upnp::Outcome::Failed(_)) => "failed",
        }
    }
}

/// What changed since the last `take_news` (the world turns it into notices and
/// `world.network`; never a log line with an address).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NatNews {
    NoStunServers,
    PunchAnswered,
    Upnp(upnp::Outcome),
    FactsChanged,
}

/// The raw facts (NetworkFacts without the world id and the bound port).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NatFacts {
    pub kind: NatKind,
    pub public: Vec<SocketAddr>,
    pub upnp: Upnp,
    pub upnp_address: Option<SocketAddr>,
    pub behind_another_nat: bool,
}

pub struct Nat {
    settings: NatSettings,
    socket_is_v6: bool,
    resolved: Option<Receiver<Vec<SocketAddr>>>,
    servers: Vec<SocketAddr>,
    last_round: Option<u64>,
    /// The last round or keepalive, and the IPv4 server the last keepalive went to.
    last_keepalive: Option<u64>,
    keepalive_server: usize,
    /// (transaction, server, sent)
    pending: Vec<(TxId, SocketAddr, u64)>,
    /// (server, our address as it saw it): the last answer of each server.
    seen: Vec<(SocketAddr, SocketAddr)>,
    /// (player, sends left, next send, transaction)
    punches: Vec<(SocketAddr, u8, u64, TxId)>,
    upnp_rx: Option<Receiver<upnp::Outcome>>,
    upnp: Upnp,
    answers: (u64, u32),
    last_facts: Option<NatFacts>,
    news: Vec<NatNews>,
}

impl Nat {
    /// Starts the STUN servers' lookup and the router's mapping (each in a thread), as asked.
    pub fn new(settings: NatSettings, socket_is_v6: bool, now_ms: u64) -> Nat {
        let resolved = if settings.discovery {
            let (tx, rx) = mpsc::channel();
            let names = settings.stun_servers.clone();
            std::thread::spawn(move || {
                let mut out = Vec::new();
                for n in names {
                    // one IPv4 address per server (the NAT's kind compares IPv4 ports), plus one IPv6
                    if let Ok(addrs) = n.to_socket_addrs() {
                        let addrs: Vec<SocketAddr> = addrs.collect();
                        out.extend(addrs.iter().find(|a| a.is_ipv4()).copied());
                        out.extend(addrs.iter().find(|a| a.is_ipv6()).copied());
                    }
                }
                let _ = tx.send(out);
            });
            Some(rx)
        } else {
            None
        };
        let (upnp_rx, state) = if settings.upnp {
            let (tx, rx) = mpsc::channel();
            let port = settings.port;
            std::thread::spawn(move || upnp::run(port, |o| drop(tx.send(o))));
            (Some(rx), Upnp::Pending)
        } else {
            (None, Upnp::Off)
        };
        let mut nat = Nat {
            settings,
            socket_is_v6,
            resolved,
            servers: Vec::new(),
            last_round: None,
            last_keepalive: None,
            keepalive_server: 0,
            pending: Vec::new(),
            seen: Vec::new(),
            punches: Vec::new(),
            upnp_rx,
            upnp: state,
            answers: (now_ms, 0),
            last_facts: None,
            news: Vec::new(),
        };
        nat.last_facts = Some(nat.facts());
        nat
    }

    /// A datagram the world received. True when it was STUN (handled here; never NerdCore).
    pub fn on_packet(
        &mut self,
        send: &mut dyn FnMut(SocketAddr, Vec<u8>),
        data: &[u8],
        from: SocketAddr,
        now_ms: u64,
    ) -> bool {
        if !stun::is_stun(data) {
            return false;
        }
        let Some(m) = stun::parse(data) else {
            return true;
        };
        match m.kind {
            stun::BINDING_SUCCESS => {
                let from = stun::canonical(from);
                if let Some(i) = self
                    .pending
                    .iter()
                    .position(|(t, s, _)| *t == m.txid && *s == from)
                {
                    self.pending.swap_remove(i);
                    if let Some(mapped) = m.mapped {
                        self.seen.retain(|(s, _)| *s != from);
                        self.seen.push((from, stun::canonical(mapped)));
                        self.check_facts();
                    }
                } else if let Some(p) = self
                    .punches
                    .iter_mut()
                    .find(|p| p.3 == m.txid && stun::canonical(p.0) == from)
                {
                    // the player's proxy answered our punch: the path is open both ways
                    p.1 = 0;
                    self.news.push(NatNews::PunchAnswered);
                }
            }
            stun::BINDING_REQUEST => {
                if now_ms.saturating_sub(self.answers.0) >= 1000 {
                    self.answers = (now_ms, 0);
                }
                if self.answers.1 < ANSWERS_PER_SECOND {
                    self.answers.1 += 1;
                    send(from, stun::binding_success(&m.txid, from));
                }
            }
            _ => {}
        }
        true
    }

    /// A lookup or a UPnP mapping is still running in its thread (`tick` polls its result).
    pub fn polling(&self) -> bool {
        self.resolved.is_some() || self.upnp_rx.is_some()
    }

    /// The next time `tick` has something to do, if any.
    pub fn next_deadline(&self) -> Option<u64> {
        let round = if self.servers.is_empty() {
            None
        } else {
            Some(
                self.last_round
                    .map_or(0, |t| t + self.settings.stun_every_ms),
            )
        };
        let keepalive = self
            .last_keepalive
            .filter(|_| self.settings.keepalive_ms > 0 && self.servers.iter().any(|s| s.is_ipv4()))
            .map(|t| t + self.settings.keepalive_ms);
        let punch = self.punches.iter().filter(|p| p.1 > 0).map(|p| p.2).min();
        [round, keepalive, punch].into_iter().flatten().min()
    }

    /// The STUN servers (the lookup's result; tests set them directly): at most 16, canonical,
    /// each once.
    pub fn set_servers(&mut self, servers: &[SocketAddr]) {
        let mut out: Vec<SocketAddr> = Vec::new();
        for s in servers
            .iter()
            .take(SERVERS_MAX)
            .map(|s| stun::canonical(*s))
        {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        self.servers = out;
    }

    /// The timer: STUN rounds, punches, the router's news. Lookup and UPnP results are polled
    /// here too, so the caller ticks it at least every second while the world is open.
    pub fn tick(&mut self, send: &mut dyn FnMut(SocketAddr, Vec<u8>), now_ms: u64) {
        if let Some(rx) = &self.resolved
            && let Ok(servers) = rx.try_recv()
        {
            if servers.is_empty() {
                self.news.push(NatNews::NoStunServers);
            }
            self.set_servers(&servers);
            self.resolved = None;
        }
        self.pending
            .retain(|(_, _, sent)| now_ms.saturating_sub(*sent) < PENDING_TTL_MS);
        if !self.servers.is_empty()
            && self
                .last_round
                .is_none_or(|t| now_ms.saturating_sub(t) >= self.settings.stun_every_ms)
        {
            self.last_round = Some(now_ms);
            self.last_keepalive = Some(now_ms);
            for &server in &self.servers {
                if server.is_ipv6() && !self.socket_is_v6 {
                    continue;
                }
                let t = stun::new_txid();
                if self.pending.len() < PENDING_MAX {
                    self.pending.push((t, server, now_ms));
                }
                send(
                    stun::for_socket(server, self.socket_is_v6),
                    stun::binding_request(&t).to_vec(),
                );
            }
        } else if self.settings.keepalive_ms > 0
            && self
                .last_keepalive
                .is_some_and(|t| now_ms.saturating_sub(t) >= self.settings.keepalive_ms)
            && self.pending.len() < PENDING_MAX
        {
            // between rounds, one IPv4 server in turn keeps the mapping open
            let v4: Vec<SocketAddr> = self
                .servers
                .iter()
                .copied()
                .filter(|s| s.is_ipv4())
                .collect();
            if !v4.is_empty() {
                self.last_keepalive = Some(now_ms);
                self.keepalive_server = (self.keepalive_server + 1) % v4.len();
                let server = v4[self.keepalive_server];
                let t = stun::new_txid();
                self.pending.push((t, server, now_ms));
                send(
                    stun::for_socket(server, self.socket_is_v6),
                    stun::binding_request(&t).to_vec(),
                );
            }
        }
        for p in self.punches.iter_mut() {
            if p.1 > 0 && now_ms >= p.2 {
                send(
                    stun::for_socket(p.0, self.socket_is_v6),
                    stun::binding_request(&p.3).to_vec(),
                );
                p.1 -= 1;
                p.2 = now_ms + PUNCH_GAP_MS;
            }
        }
        self.punches.retain(|p| p.1 > 0);
        if let Some(rx) = &self.upnp_rx {
            let mut latest = None;
            while let Ok(o) = rx.try_recv() {
                latest = Some(o);
            }
            if let Some(o) = latest
                && self.upnp != Upnp::Done(o.clone())
            {
                self.news.push(NatNews::Upnp(o.clone()));
                self.upnp = Upnp::Done(o);
                self.check_facts();
            }
        }
    }

    /// A burst of Binding requests to a joining player for a couple of seconds.
    pub fn punch(&mut self, to: SocketAddr, now_ms: u64) {
        let to = stun::canonical(to);
        if to.port() == 0 || to.ip().is_unspecified() || to.ip().is_multicast() {
            return;
        }
        if let Some(p) = self.punches.iter_mut().find(|p| p.0 == to) {
            p.1 = PUNCH_SENDS;
            return;
        }
        if self.punches.len() >= PUNCHES_MAX {
            self.punches.remove(0);
        }
        self.punches
            .push((to, PUNCH_SENDS, now_ms, stun::new_txid()));
    }

    pub fn kind(&self) -> NatKind {
        let v4: Vec<&SocketAddr> = self
            .seen
            .iter()
            .filter(|(s, _)| s.is_ipv4())
            .map(|(_, m)| m)
            .collect();
        match v4.as_slice() {
            [a, b, ..] if a.port() == b.port() && a.ip() == b.ip() => NatKind::Cone,
            [_, _, ..] => NatKind::Symmetric,
            _ => NatKind::Unknown,
        }
    }

    /// The public addresses STUN saw (one per family).
    pub fn public(&self) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = Vec::new();
        for (_, m) in &self.seen {
            if !out.iter().any(|o| o.is_ipv4() == m.is_ipv4()) {
                out.push(*m);
            }
        }
        out
    }

    pub fn facts(&self) -> NatFacts {
        let upnp_address = match &self.upnp {
            Upnp::Done(upnp::Outcome::Mapped {
                external_ip,
                external_port,
                ..
            }) => Some(SocketAddr::new(*external_ip, *external_port)),
            _ => None,
        };
        NatFacts {
            kind: self.kind(),
            public: self.public(),
            upnp: self.upnp.clone(),
            behind_another_nat: upnp_address.is_some_and(|a| upnp::behind_another_nat(a.ip())),
            upnp_address,
        }
    }

    fn check_facts(&mut self) {
        let facts = self.facts();
        if self.last_facts.as_ref() != Some(&facts) {
            self.last_facts = Some(facts);
            self.news.push(NatNews::FactsChanged);
        }
    }

    pub fn take_news(&mut self) -> Vec<NatNews> {
        std::mem::take(&mut self.news)
    }

    /// The router's external address, when it forwarded the world.
    pub fn upnp_address(&self) -> Option<IpAddr> {
        self.facts().upnp_address.map(|a| a.ip())
    }
}
