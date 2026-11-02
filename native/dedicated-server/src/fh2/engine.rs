// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! FH2's engines of a world (contract §7): free roam (made at `world.open`), a car meet per
//! lobby type 3 and 6 (made on demand), Road Trip (lobby type 1, made on demand, only with a
//! catalog that has events).
//!
//! **Step 1 scaffold.** `Engine` has the shape the world and the loop drive (receive, tick, its
//! deadline, its counts) but no game behaviour: it admits nobody and drops every datagram, which
//! is exactly what a world does before its admission is ported (fail closed, contract §2.2). The
//! features are ported one by one against golden vectors (README, PORT_STATUS).

use std::net::SocketAddr;

use super::settings::RelaySettings;
use crate::core::game::Send;

/// One engine kind of a world (the `engine` field of logs and events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EngineKind {
    FreeRoam,
    CarMeet3,
    CarMeet6,
    RoadTrip,
}

impl EngineKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::FreeRoam => "freeroam",
            EngineKind::CarMeet3 => "carmeet3",
            EngineKind::CarMeet6 => "carmeet6",
            EngineKind::RoadTrip => "roadtrip",
        }
    }
}

/// The seeds an engine draws at creation (weather, tag, team), fixed by the settings or drawn
/// from the world's RNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seeds {
    pub weather: i32,
    pub tag: i32,
    pub team: i32,
}

#[derive(Debug)]
pub struct Engine {
    pub kind: EngineKind,
    pub settings: RelaySettings,
    pub seeds: Seeds,
    /// Synced peers (the host's loopback game included).
    synced: usize,
    /// Admitted, not synced yet, and held opens.
    handshaking: usize,
    /// Datagrams this engine dropped (unknown peers, step 1: every datagram).
    pub dropped: u64,
}

impl Engine {
    pub fn new(kind: EngineKind, settings: RelaySettings, seeds: Seeds) -> Engine {
        Engine {
            kind,
            settings,
            seeds,
            synced: 0,
            handshaking: 0,
            dropped: 0,
        }
    }

    /// A datagram from `from` (canonical endpoint) at `now_ms`. Step 1: nobody is admitted, so
    /// it is dropped.
    pub fn receive(&mut self, _data: &[u8], _from: SocketAddr, _now_ms: u64, _out: &mut Vec<Send>) {
        self.dropped += 1;
    }

    /// The 20 ms tick. Never called for an idle engine (`is_idle`); a no-op when nothing is
    /// pending, which a golden vector proves for the C# engine (contract §5.1 "Ticks").
    pub fn tick(&mut self, _now_ms: u64, _out: &mut Vec<Send>) {}

    /// No peer, no held open, nothing pending: no tick.
    pub fn is_idle(&self) -> bool {
        self.synced == 0 && self.handshaking == 0
    }

    pub fn synced(&self) -> usize {
        self.synced
    }

    pub fn handshaking(&self) -> usize {
        self.handshaking
    }

    /// Drops every peer of `xuid` (`world.kick`); the count dropped.
    pub fn kick(&mut self, _xuid: u64, _now_ms: u64, _out: &mut Vec<Send>) -> usize {
        0
    }

    /// Sends the game's Close to every peer (drain, shutdown). Step 1: none.
    pub fn close_all(&mut self, _out: &mut Vec<Send>) {}
}
