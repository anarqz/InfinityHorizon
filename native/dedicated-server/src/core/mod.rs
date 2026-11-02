// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The deterministic, I/O-free, game-agnostic server (contract §4-§8): control commands, worlds, admission
//! state, timers and events. The caller supplies the monotonic clock (`now_ms`), the random seed
//! and an `Io` for what must touch the system (binding a private world's socket, reading memory
//! and CPU); datagrams leave through `Server::take_datagrams`, events through
//! `Server::pop_event_line`. The runtime's loop and the golden-vector runner drive the same API.

pub mod events;
pub mod fields;
pub mod game;
pub mod keys;
pub mod rng;
pub mod server;
pub mod timers;
pub mod world;

use std::{net::SocketAddr, sync::Arc};

pub use server::Server;

use crate::net::BindFailure;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// One world per process, its own UDP socket (the launcher's child).
    Private,
    /// Up to `maxWorlds` worlds on one loopback socket behind the co-located relay (the VM).
    Public,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Private => "private",
            Profile::Public => "public",
        }
    }

    pub fn parse(s: &str) -> Option<Profile> {
        match s {
            "private" => Some(Profile::Private),
            "public" => Some(Profile::Public),
            _ => None,
        }
    }
}

/// A socket of the process: the public profile's loopback socket, or a private world's.
pub type SockId = u32;
pub const PUBLIC_SOCKET: SockId = 0;

/// A datagram to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub sock: SockId,
    pub to: SocketAddr,
    pub data: Vec<u8>,
}

/// A private world's socket, bound by the `Io`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundWorld {
    pub sock: SockId,
    pub port: u16,
    pub dual: bool,
}

/// What the server needs from the system.
pub trait Io {
    /// Binds a private world's UDP socket on `port` (contract §5.1); never another port.
    fn bind_world(&mut self, port: u16) -> Result<BoundWorld, BindFailure>;
    /// The world closed: its socket goes.
    fn release_world(&mut self, sock: SockId);
    /// Live heap bytes (the counting allocator).
    fn memory_used_bytes(&self) -> u64;
    /// The process's CPU time so far, when the system tells.
    fn cpu_ms(&self) -> Option<u64>;
    /// Resident set size, when the system tells.
    fn rss_bytes(&self) -> Option<u64>;
}

/// The server's settings (from the CLI, contract §2).
#[derive(Clone)]
pub struct Config {
    pub profile: Profile,
    pub max_worlds: usize,
    pub max_seats: usize,
    /// 0: no budget (private default).
    pub memory_limit_mb: u64,
    pub drain_seconds: u64,
    /// 0: off.
    pub stats_seconds: u64,
    pub keys: keys::KeyRing,
    /// The game modules this server hosts (OD-186), by `world.open`'s `game`.
    pub games: Vec<Arc<dyn game::Game>>,
    /// The co-located relay's endpoint (public): the only source the public socket accepts.
    pub relay: SocketAddr,
    /// The public socket is bound (public profile; the runtime sets it).
    pub public_socket: bool,
    /// The pipe's per-launch secret (64 lowercase hex), compared with `hello.secret`.
    pub control_secret: Option<String>,
}

impl Config {
    /// Defaults per profile, as the CLI table (contract §2).
    pub fn new(profile: Profile) -> Config {
        let public = profile == Profile::Public;
        Config {
            profile,
            max_worlds: if public { 4 } else { 1 },
            max_seats: if public { 48 } else { 12 },
            memory_limit_mb: if public { 400 } else { 0 },
            drain_seconds: 25,
            stats_seconds: 60,
            keys: keys::KeyRing::default(),
            games: Vec::new(),
            relay: SocketAddr::from(([127, 0, 0, 1], 5101)),
            public_socket: false,
            control_secret: None,
        }
    }
}
