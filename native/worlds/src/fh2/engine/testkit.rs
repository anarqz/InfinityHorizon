// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Test helpers (unit tests only): an engine that admits everyone and synced peers seated
//! directly, for checks below the wire (budgets, event rules).

use std::net::SocketAddr;

use super::{Engine, EngineKind, EngineSettings, Policy, random::EngineRandom, random::Seeds};
use crate::{core::Profile, fh2::RelaySettings};

/// Admits every identity on every endpoint.
#[derive(Debug, Default)]
pub(super) struct AllowAll;

impl Policy for AllowAll {
    fn has_authorize(&self) -> bool {
        false
    }
    fn has_authorize_hook(&self) -> bool {
        false
    }
    fn authorize(&mut self, _: u64, _: SocketAddr, _: i64) -> bool {
        true
    }
    fn authorize_hook(&mut self, _: u64, _: SocketAddr, _: i64) -> bool {
        true
    }
    fn awaits_proof(&mut self, _: u64, _: i64) -> Option<bool> {
        None
    }
}

/// A free-roam engine with events, the private profile's limits.
pub(super) fn engine() -> Engine {
    let settings = EngineSettings {
        relay: RelaySettings::for_profile(Profile::Private),
        max_players: 12,
        races: true,
        car_meet_lobby: None,
        enforce_car_rule: false,
        road_trip: None,
    };
    let seeds = Seeds {
        weather: 1,
        tag: 2,
        team: 3,
        attempt: 4,
        transaction: 5,
    };
    Engine::new(
        EngineKind::FreeRoam,
        settings,
        EngineRandom::new(seeds),
        Box::new(AllowAll),
    )
    .expect("valid settings")
}

/// Seats a synced peer at index `index` (endpoint 192.0.2.<index>:3074); its serial.
pub(super) fn seat(e: &mut Engine, index: u8, xuid: u64) -> u64 {
    let serial = e.next_serial;
    e.next_serial += 1;
    let endpoint = SocketAddr::from(([192, 0, 2, index], 3074));
    let mut p = e.new_peer(serial, endpoint, index, xuid, 1, 1);
    p.synced = true;
    e.peers.push(p);
    serial
}
