// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Relay legs (CR-S57, contract §5.1, MATCHMAKING_API §5b): the FIR1 rendezvous HOST role
//! spoken from the world's socket (`"FIR1" ‖ kind ‖ body`; every relay → host body and the
//! `relay`/`data` bodies start with the 16-byte grant id). One leg per verified grant that names a relay: a `hello` (role
//! 2, the grant's exact claims bytes and signature) at once and every second until the relay
//! answers, again with its challenge's cookie, then every 15 s; `relay` every second while the
//! punch says the pair is relayed, until `relay ok`. A leg unanswered for 60 s goes quiet. Each
//! relayed joiner is a virtual peer `fd46:4952:<grant[0..12]>`, port 1: the relay's `data` comes
//! from it, and what the engine sends it leaves as `data`. Only the relay a verified grant names
//! is contacted, and only its datagrams about that grant are read. Ported from the C#
//! `RelayLegs` (client f81d09c).

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use super::game::Send;

pub const MAGIC: &[u8; 4] = b"FIR1";
pub const MAX_PAYLOAD: usize = 1200;
const HELLO_EVERY_MS: u64 = 15_000;
const HELLO_RETRY_MS: u64 = 1000;
const RELAY_EVERY_MS: u64 = 1000;
const SILENT_AFTER_MS: u64 = 60_000;
pub const MAX_LEGS: usize = 64;
const VIRTUAL_PREFIX: [u8; 4] = [0xfd, 0x46, 0x49, 0x52];

/// One grant to meet a joiner for at a relay: the verified grant's exact claims and signature,
/// the relay's rendezvous endpoint, and whether the pair goes through it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegSpec {
    pub grant: [u8; 16],
    pub claims: Vec<u8>,
    pub signature: Vec<u8>,
    pub relay: SocketAddr,
    pub forward: bool,
}

#[derive(Debug)]
struct Leg {
    spec: LegSpec,
    virtual_peer: SocketAddr,
    cookie: Option<[u8; 16]>,
    last_hello: Option<u64>,
    last_relay: Option<u64>,
    last_heard: u64,
    answered: bool,
    ok: bool,
    unavailable: bool,
}

/// What a datagram was to the legs.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    NotOurs,
    Handled,
    /// The joiner's datagram, from its virtual peer.
    Data(SocketAddr, Vec<u8>),
}

/// A virtual peer's endpoint (never routable, never sent to).
pub fn virtual_for(grant: &[u8; 16]) -> SocketAddr {
    let mut a = [0u8; 16];
    a[..4].copy_from_slice(&VIRTUAL_PREFIX);
    a[4..].copy_from_slice(&grant[..12]);
    SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a)), 1)
}

pub fn is_virtual(e: SocketAddr) -> bool {
    matches!(e.ip(), IpAddr::V6(v) if v.octets()[..4] == VIRTUAL_PREFIX)
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + body.len());
    f.extend_from_slice(MAGIC);
    f.push(kind);
    f.extend_from_slice(body);
    f
}

/// The legs of one world.
#[derive(Debug, Default)]
pub struct Legs {
    legs: Vec<Leg>,
}

impl Legs {
    pub fn len(&self) -> usize {
        self.legs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.legs.is_empty()
    }

    /// Whether the pair of `grant` is carried by its relay now (`relay ok`).
    pub fn relayed(&self, grant: &[u8; 16]) -> bool {
        self.legs.iter().any(|l| l.spec.grant == *grant && l.ok)
    }

    /// Whether this world holds a leg for `grant` at `relay` (the public socket's multiplex).
    pub fn owns(&self, grant: &[u8], relay: SocketAddr) -> bool {
        self.legs
            .iter()
            .any(|l| l.spec.relay == relay && l.spec.grant[..] == *grant)
    }

    /// The grants that name a relay now; legs no longer listed go, new ones say hello at once.
    pub fn update(&mut self, specs: Vec<LegSpec>, now: u64, out: &mut Vec<Send>) {
        let mut wanted: Vec<LegSpec> = Vec::new();
        for s in specs {
            if s.relay.port() > 0
                && !wanted.iter().any(|w| w.grant == s.grant)
                && wanted.len() < MAX_LEGS
            {
                wanted.push(s);
            }
        }
        self.legs
            .retain(|l| wanted.iter().any(|w| w.grant == l.spec.grant));
        for spec in wanted {
            if let Some(leg) = self
                .legs
                .iter_mut()
                .find(|l| l.spec.grant == spec.grant && l.spec.relay == spec.relay)
            {
                // the joiner moved to the relay since: a quiet leg tries again at once
                if spec.forward && !leg.spec.forward {
                    leg.last_heard = now;
                    leg.last_hello = None;
                    leg.last_relay = None;
                    leg.unavailable = false;
                }
                leg.spec = spec;
                continue;
            }
            self.legs.retain(|l| l.spec.grant != spec.grant);
            self.legs.push(Leg {
                virtual_peer: virtual_for(&spec.grant),
                spec,
                cookie: None,
                last_hello: None,
                last_relay: None,
                last_heard: now,
                answered: false,
                ok: false,
                unavailable: false,
            });
        }
        self.tick(now, out);
    }

    pub fn clear(&mut self) {
        self.legs.clear();
    }

    /// The legs' timers: hellos and `relay`.
    pub fn tick(&mut self, now: u64, out: &mut Vec<Send>) {
        for leg in &mut self.legs {
            if now.saturating_sub(leg.last_heard) >= SILENT_AFTER_MS {
                continue; // quiet until the relay or the punch says otherwise
            }
            let every = if leg.answered {
                HELLO_EVERY_MS
            } else {
                HELLO_RETRY_MS
            };
            if leg
                .last_hello
                .is_none_or(|t| now.saturating_sub(t) >= every)
            {
                hello(leg, now, out);
            }
            if leg.spec.forward
                && !leg.ok
                && !leg.unavailable
                && leg
                    .last_relay
                    .is_none_or(|t| now.saturating_sub(t) >= RELAY_EVERY_MS)
            {
                leg.last_relay = Some(now);
                out.push(Send {
                    to: leg.spec.relay,
                    data: frame(0x03, &leg.spec.grant),
                });
            }
        }
    }

    /// When the legs next need `tick` (none: no leg).
    pub fn next_deadline(&self, now: u64) -> Option<u64> {
        (!self.legs.is_empty()).then_some(now + 250)
    }

    /// A datagram from `from`: FIR1 from one leg's relay about that leg's grant, or not ours.
    pub fn receive(
        &mut self,
        data: &[u8],
        from: SocketAddr,
        now: u64,
        out: &mut Vec<Send>,
    ) -> Received {
        if data.len() < 5 + 16 || &data[..4] != MAGIC {
            return Received::NotOurs;
        }
        let kind = data[4];
        let body = &data[5..];
        let Some(leg) = self
            .legs
            .iter_mut()
            .find(|l| l.spec.relay == from && body[..16] == l.spec.grant)
        else {
            return Received::NotOurs;
        };
        leg.last_heard = now;
        leg.answered = true;
        match kind {
            0x07 if body.len() == 32 => {
                let mut c = [0u8; 16];
                c.copy_from_slice(&body[16..32]);
                leg.cookie = Some(c);
                hello(leg, now, out);
                Received::Handled
            }
            0x04 if body.len() == 16 => {
                leg.ok = true;
                leg.unavailable = false;
                Received::Handled
            }
            0x06 if body.len() == 16 => {
                leg.ok = false;
                leg.unavailable = true;
                Received::Handled
            }
            0x05 if body.len() > 16 && body.len() - 16 <= MAX_PAYLOAD => {
                if !leg.ok {
                    return Received::Handled; // data only after the relay confirmed the grant
                }
                Received::Data(leg.virtual_peer, body[16..].to_vec())
            }
            _ => Received::Handled,
        }
    }

    /// The engine's datagram to a relayed joiner, as `data` to its relay.
    pub fn wrap(&self, to: SocketAddr, payload: &[u8]) -> Option<Send> {
        if !is_virtual(to) || payload.is_empty() || payload.len() > MAX_PAYLOAD {
            return None;
        }
        let leg = self.legs.iter().find(|l| l.virtual_peer == to && l.ok)?;
        let mut body = Vec::with_capacity(16 + payload.len());
        body.extend_from_slice(&leg.spec.grant);
        body.extend_from_slice(payload);
        Some(Send {
            to: leg.spec.relay,
            data: frame(0x05, &body),
        })
    }
}

fn hello(leg: &mut Leg, now: u64, out: &mut Vec<Send>) {
    leg.last_hello = Some(now);
    let claims = &leg.spec.claims;
    if claims.len() > usize::from(u16::MAX) || leg.spec.signature.len() != 64 {
        return;
    }
    // the hello names its grant inside the signed claims (fi_relay::wire)
    let mut body = Vec::with_capacity(3 + claims.len() + 64 + 16);
    body.push(2); // host
    body.extend_from_slice(&(claims.len() as u16).to_be_bytes());
    body.extend_from_slice(claims);
    body.extend_from_slice(&leg.spec.signature);
    if let Some(c) = &leg.cookie {
        body.extend_from_slice(c);
    }
    out.push(Send {
        to: leg.spec.relay,
        data: frame(0x01, &body),
    });
}
