// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The side channels beside NerdCore on the world socket (contract §5.3): the hook's car state
//! (lights and roof, kind 0x3E91), and voice (0x3E93/0x3E94, [`super::voice`]); the retired
//! livery channel never reaches an engine.

use std::net::{IpAddr, SocketAddr};

use super::{Engine, SideRate};
use crate::fh2::{carstate, voice};

pub const SIDE_CHANNEL_PACKETS_PER_SECOND: f64 = 4096.0;
pub const SIDE_CHANNEL_BYTES_PER_SECOND: f64 = 2.0 * 1024.0 * 1024.0;
pub const SIDE_CHANNEL_CONTROLS_PER_SECOND: f64 = 128.0;
/// A hook's car state is taken at most once per this many milliseconds.
const CAR_STATE_MIN_MS: i64 = 50;
/// Every hook gets every car's state again this often.
const CAR_STATE_REFRESH_MS: i64 = 1000;

/// A separate hook socket may speak for a peer only from the same private IP; a public address can
/// stand for unrelated players, so it needs the exact endpoint.
pub fn may_speak_for(peer: SocketAddr, from: SocketAddr) -> bool {
    let peer = crate::stun::plain_endpoint(peer);
    let from = crate::stun::plain_endpoint(from);
    peer == from || is_private(peer.ip()) && peer.ip() == from.ip()
}

pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let b = v4.octets();
            v4.is_loopback()
                || b[0] == 10
                || b[0] == 172 && (16..=31).contains(&b[1])
                || b[0] == 192 && b[1] == 168
                || b[0] == 169 && b[1] == 254
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(IpAddr::V4(v4));
            }
            let b = v6.octets();
            v6.is_loopback() || (b[0] & 0xfe) == 0xfc || b[0] == 0xfe && (b[1] & 0xc0) == 0x80
        }
    }
}

impl Engine {
    pub(super) fn receive_side_channel(&mut self, data: &[u8], endpoint: SocketAddr) {
        match u16::from_be_bytes([data[0], data[1]]) {
            carstate::MESSAGE_TYPE => self.receive_car_state(data, endpoint),
            voice::VOICE_KIND => self.receive_voice(data, endpoint),
            voice::CONTROL_KIND => self.receive_voice_control(data, endpoint),
            _ => {}
        }
    }

    pub(super) fn take_side_rate(&mut self, serial: u64, bytes: usize, control: bool) -> bool {
        let now = self.now;
        if !self.side_rates.contains_key(&serial) {
            self.side_rates.insert(
                serial,
                SideRate {
                    at: now,
                    packets: SIDE_CHANNEL_PACKETS_PER_SECOND * 2.0,
                    bytes: SIDE_CHANNEL_BYTES_PER_SECOND * 2.0,
                    controls: SIDE_CHANNEL_CONTROLS_PER_SECOND * 2.0,
                },
            );
        }
        let Some(r) = self.side_rates.get_mut(&serial) else {
            return false;
        };
        let elapsed = (now - r.at).max(0) as f64 / 1000.0;
        r.at = now;
        r.packets = (SIDE_CHANNEL_PACKETS_PER_SECOND * 2.0)
            .min(r.packets + elapsed * SIDE_CHANNEL_PACKETS_PER_SECOND);
        r.bytes = (SIDE_CHANNEL_BYTES_PER_SECOND * 2.0)
            .min(r.bytes + elapsed * SIDE_CHANNEL_BYTES_PER_SECOND);
        r.controls = (SIDE_CHANNEL_CONTROLS_PER_SECOND * 2.0)
            .min(r.controls + elapsed * SIDE_CHANNEL_CONTROLS_PER_SECOND);
        if r.packets < 1.0 || r.bytes < bytes as f64 || control && r.controls < 1.0 {
            self.side_channel_rate_drops += 1;
            return false;
        }
        r.packets -= 1.0;
        r.bytes -= bytes as f64;
        if control {
            r.controls -= 1.0;
        }
        true
    }

    fn receive_car_state(&mut self, data: &[u8], endpoint: SocketAddr) {
        let Some(up) = carstate::read_report(data) else {
            return;
        };
        let by_endpoint = self
            .peers
            .iter()
            .find(|p| p.endpoint == endpoint)
            .map(|p| (p.serial, p.xuid));
        if let Some((_, xuid)) = by_endpoint
            && xuid != up.xuid
        {
            return;
        }
        let mut serial = by_endpoint.map(|x| x.0);
        if serial.is_none() {
            let candidates: Vec<u64> = self
                .peers
                .iter()
                .filter(|p| p.synced && p.xuid == up.xuid)
                .map(|p| p.serial)
                .collect();
            for c in candidates {
                if self.hook_source(c, endpoint) {
                    serial = Some(c);
                    break;
                }
            }
        }
        let Some(serial) = serial else { return };
        let Some(p) = self.peer(serial) else { return };
        let (synced, xuid, peer_endpoint) = (p.synced, p.xuid, p.endpoint);
        if !synced
            || !self.allowed(xuid, peer_endpoint)
            || !self.allowed_hook(xuid, endpoint)
            || !self.take_side_rate(serial, data.len(), true)
        {
            return;
        }
        let now = self.now;
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        if p.car_state_at.is_some_and(|at| now - at < CAR_STATE_MIN_MS) {
            return;
        }
        p.car_state_at = Some(now);
        p.last_seen = now;
        p.car_hook = Some(endpoint);
        let changed = p
            .car_state
            .is_none_or(|s| s.headlights != up.look.headlights || s.roof_mode != up.look.roof_mode);
        p.car_state = Some(up.look);
        if changed {
            self.pass_car_state(serial);
        }
    }

    fn pass_car_state(&mut self, from: u64) {
        let Some(p) = self.peer(from) else { return };
        let (state, xuid, endpoint, index, position, hook) = (
            p.car_state,
            p.xuid,
            p.endpoint,
            p.index,
            p.car_position,
            p.car_hook,
        );
        let Some(state) = state else { return };
        if !self.allowed(xuid, endpoint) {
            return;
        }
        let Some(source) = hook else { return };
        if !self.allowed_hook(xuid, source) {
            return;
        }
        let txid = self.random.transaction();
        let packet = carstate::encode_peer_look_with(
            &carstate::PeerLook {
                look: state,
                peer_index: index,
                xuid,
                world_position: position,
            },
            &txid,
        );
        let receivers: Vec<(u64, u64, SocketAddr, bool, Option<SocketAddr>)> = self
            .peers
            .iter()
            .map(|p| (p.serial, p.xuid, p.endpoint, p.synced, p.car_hook))
            .collect();
        for (s, x, e, synced, car_hook) in receivers {
            if s == from || !synced || !self.allowed(x, e) {
                continue;
            }
            let Some(hook) = car_hook else { continue };
            if self.allowed_hook(x, hook) {
                self.emit(hook, packet.clone());
            }
        }
    }

    pub(super) fn tick_side_channels(&mut self) {
        if self.now - self.last_car_states >= CAR_STATE_REFRESH_MS {
            self.last_car_states = self.now;
            let serials: Vec<u64> = self.peers.iter().map(|p| p.serial).collect();
            for s in serials {
                self.pass_car_state(s);
            }
        }
        self.tick_voice();
    }
}
