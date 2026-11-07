// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The deterministic, I/O-free, game-agnostic server (contract §4-§8): control commands, worlds, admission
//! state, timers and events. The caller supplies the monotonic clock (`now_ms`), the random seed
//! and an `Io` for what must touch the system (reading memory and CPU); datagrams for the one
//! public socket leave through `Server::take_datagrams`, events through
//! `Server::pop_event_line`. The runtime's loop and the golden-vector runner drive the same API.
//!
//! Official worlds only (OD-208; WORLDS_OFFICIAL.md §6): up to `maxWorlds` worlds on one public
//! UDP socket of a Worlds host VM, joiners straight to it (OD-189); no world socket of its own,
//! no STUN to a host, no UPnP, no punching, no relay legs.

pub mod admission;
pub mod events;
pub mod fields;
pub mod fiup;
pub mod game;
pub mod grant;
pub mod keys;
pub mod rng;
pub mod server;
pub mod timers;
pub mod world;

#[cfg(test)]
mod official_tests;

use std::{net::SocketAddr, sync::Arc};

pub use server::Server;

/// Bytes one heap allocation costs beyond its payload (allocator header and size-class
/// rounding): the rule of the worst-case memory accounting (contract §8).
pub const ALLOC_SLACK_BYTES: usize = 32;

/// The longest datagram FM5's race server packs its options into (`fm5::race::PACKET_MAX_BYTES`;
/// formerly the relay legs' payload bound): below the games' 1,372-byte packet limit (lobby spec
/// §13.8), so a packet that grows by a message never reaches it.
pub const MAX_PAYLOAD: usize = 1200;

/// The most one world's game-agnostic tables hold (admission, FIUP), every table at its cap
/// (contract §8's worst-case table).
pub fn world_tables_worst_case_bytes() -> usize {
    admission::Gate::worst_case_bytes()
}

/// A datagram to send on the public socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub to: SocketAddr,
    pub data: Vec<u8>,
}

/// What the server needs from the system.
pub trait Io {
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
    pub max_worlds: usize,
    pub max_seats: usize,
    /// 0: no budget (never from the CLI, which takes 128-65536).
    pub memory_limit_mb: u64,
    pub drain_seconds: u64,
    /// 0: off.
    pub stats_seconds: u64,
    pub keys: keys::KeyRing,
    /// The game modules this server hosts (OD-186), by `world.open`'s `game`.
    pub games: Vec<Arc<dyn game::Game>>,
    /// The public socket is bound (the runtime sets it).
    pub public_socket: bool,
}

impl Default for Config {
    /// The CLI table's defaults (contract §2).
    fn default() -> Config {
        Config {
            max_worlds: 4,
            max_seats: 48,
            memory_limit_mb: 400,
            drain_seconds: 25,
            stats_seconds: 60,
            keys: keys::KeyRing::default(),
            games: Vec::new(),
            public_socket: false,
        }
    }
}

impl Config {
    pub fn new() -> Config {
        Config::default()
    }
}
