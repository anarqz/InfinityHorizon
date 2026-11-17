// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The side channels beside NerdCore on the world socket (contract §5.3): the hook's car state
//! (lights and roof, kind 0x3E91, with MATCHMAKING_API §5f's parts, signals and mod state), and
//! voice (0x3E93/0x3E94, [`super::voice`]); the retired livery channel never reaches an engine.
//!
//! §5f's gate (OD-248, fail closed): the core `0xC0F1` reaches every hook as before; `0xC0FC`
//! goes from A to B only when both signed sync sets hold [`carstate::SYNC_PARTS`], a blob of id M
//! only when both hold M, and nothing new ever goes to a hook that never sent `0xC0FC`/`0xC0FD`
//! itself (an older hook gets the layout-1 datagram byte for byte).

use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
};

use super::{Engine, SideRate};
use crate::{
    core::grant::SyncSet,
    fh2::{
        carstate::{self, ModBlob, Parts},
        voice,
    },
};

pub const SIDE_CHANNEL_PACKETS_PER_SECOND: f64 = 4096.0;
pub const SIDE_CHANNEL_BYTES_PER_SECOND: f64 = 2.0 * 1024.0 * 1024.0;
pub const SIDE_CHANNEL_CONTROLS_PER_SECOND: f64 = 128.0;
/// A hook's car state is taken at most once per this many milliseconds (the accept floor).
const CAR_STATE_MIN_MS: i64 = 50;
/// Change-triggered passes of one source are coalesced to one per this many milliseconds
/// (≤ 10/s), always with the newest state.
const CAR_STATE_PASS_MS: i64 = 100;
/// Every hook gets every car's state again this often.
const CAR_STATE_REFRESH_MS: i64 = 1000;
/// A source's blob is forgotten this long after that source last sent it.
const CAR_MOD_TTL_MS: i64 = 5000;

/// What `changed` compares: lights, roof, parts and signals, and every blob.
#[derive(PartialEq, Eq)]
struct Shown {
    headlights: u8,
    roof_mode: u8,
    parts: Option<Parts>,
    mods: Vec<ModBlob>,
}

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

type ModSlots = [Option<(ModBlob, i64)>; carstate::MODS_PER_DATAGRAM];

/// Keeps `blob` as its source's newest of that sync id. A source keeps at most two ids (one
/// datagram's worth, so a pass never splits); a third id takes the slot sent least recently.
fn keep_blob(slots: &mut ModSlots, blob: ModBlob, now: i64) {
    let at = slots
        .iter()
        .position(|s| s.is_some_and(|(b, _)| b.id == blob.id))
        .or_else(|| {
            slots
                .iter()
                .position(|s| s.is_none_or(|(_, at)| now - at >= CAR_MOD_TTL_MS))
        })
        .or_else(|| (0..slots.len()).min_by_key(|i| slots[*i].map_or(i64::MIN, |(_, at)| at)));
    if let Some(i) = at {
        slots[i] = Some((blob, now));
    }
}

/// The source's blobs not yet forgotten, by sync id.
fn live_mods(p: &super::Peer, now: i64) -> Vec<ModBlob> {
    let mut mods: Vec<ModBlob> = p
        .car_mods
        .iter()
        .flatten()
        .filter(|(_, at)| now - at < CAR_MOD_TTL_MS)
        .map(|(b, _)| *b)
        .collect();
    mods.sort_by_key(|b| b.id);
    mods
}

fn shown(p: &super::Peer, look: carstate::CarLook, now: i64) -> Shown {
    Shown {
        headlights: look.headlights,
        roof_mode: look.roof_mode,
        parts: p.car_parts,
        mods: live_mods(p, now),
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
        let own = self.sync_set(xuid);
        let ext = carstate::read_extensions(data, up.look.roof_mode);
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        if p.car_state_at.is_some_and(|at| now - at < CAR_STATE_MIN_MS) {
            return;
        }
        let before = p.car_state.map(|s| shown(p, s, now));
        p.car_state_at = Some(now);
        p.last_seen = now;
        // a new hook socket speaks for itself: what the old one said of §5f does not carry over
        p.car_speaks = ext.speaks || p.car_speaks && p.car_hook == Some(endpoint);
        p.car_hook = Some(endpoint);
        p.car_state = Some(up.look);
        // only what the source's own signed set holds is kept (the world compares ids only)
        p.car_parts = ext.parts.filter(|_| own.contains(carstate::SYNC_PARTS));
        for blob in ext.mods().filter(|b| own.contains(b.id)) {
            keep_blob(&mut p.car_mods, *blob, now);
        }
        let after = shown(p, up.look, now);
        if before.is_none_or(|b| b != after) {
            if p.car_passed_at
                .is_some_and(|at| now - at < CAR_STATE_PASS_MS)
            {
                p.car_pending = true;
            } else {
                self.pass_changed(serial);
            }
        }
    }

    /// CR-L53: the world's signed mod-sync sets, replacing the last ones (admission snapshot).
    pub fn set_sync_sets(&mut self, sets: &BTreeMap<u64, SyncSet>) {
        self.sync_sets = sets.clone();
    }

    fn sync_set(&self, xuid: u64) -> SyncSet {
        self.sync_sets.get(&xuid).copied().unwrap_or_default()
    }

    /// A change-triggered pass: counted against the source's ≤ 10/s.
    fn pass_changed(&mut self, from: u64) {
        let now = self.now;
        if let Some(p) = self.peer_mut(from) {
            p.car_passed_at = Some(now);
            p.car_pending = false;
        }
        self.pass_car_state(from);
    }

    fn pass_car_state(&mut self, from: u64) {
        let now = self.now;
        let Some(p) = self.peer(from) else { return };
        let (state, xuid, endpoint, index, position, hook, parts) = (
            p.car_state,
            p.xuid,
            p.endpoint,
            p.index,
            p.car_position,
            p.car_hook,
            p.car_parts,
        );
        let mods: Vec<ModBlob> = live_mods(p, now);
        let Some(state) = state else { return };
        if !self.allowed(xuid, endpoint) {
            return;
        }
        let Some(source) = hook else { return };
        if !self.allowed_hook(xuid, source) {
            return;
        }
        let from_set = self.sync_set(xuid);
        let txid = self.random.transaction();
        let look = carstate::PeerLook {
            look: state,
            peer_index: index,
            xuid,
            world_position: position,
        };
        let core = carstate::encode_peer_look_with(&look, &txid);
        let receivers: Vec<(u64, u64, SocketAddr, bool, Option<SocketAddr>, bool)> = self
            .peers
            .iter()
            .map(|p| {
                (
                    p.serial,
                    p.xuid,
                    p.endpoint,
                    p.synced,
                    p.car_hook,
                    p.car_speaks,
                )
            })
            .collect();
        for (s, x, e, synced, car_hook, speaks) in receivers {
            if s == from || !synced || !self.allowed(x, e) {
                continue;
            }
            let Some(hook) = car_hook else { continue };
            if !self.allowed_hook(x, hook) {
                continue;
            }
            let both = from_set.intersect(&self.sync_set(x));
            let (parts, mods) = if speaks {
                (
                    parts.filter(|_| both.contains(carstate::SYNC_PARTS)),
                    mods.iter()
                        .filter(|b| both.contains(b.id))
                        .copied()
                        .collect(),
                )
            } else {
                (None, Vec::new())
            };
            let packet = if parts.is_none() && mods.is_empty() {
                core.clone()
            } else {
                carstate::encode_peer_state_with(&look, parts, &mods, &txid)
            };
            self.emit(hook, packet);
        }
    }

    pub(super) fn tick_side_channels(&mut self) {
        let now = self.now;
        if now - self.last_car_states >= CAR_STATE_REFRESH_MS {
            self.last_car_states = now;
            let serials: Vec<u64> = self.peers.iter().map(|p| p.serial).collect();
            for s in serials {
                // the refresh carries the newest state: nothing stays pending behind it
                if let Some(p) = self.peer_mut(s) {
                    p.car_pending = false;
                    p.car_mods = p
                        .car_mods
                        .map(|m| m.filter(|(_, at)| now - at < CAR_MOD_TTL_MS));
                }
                self.pass_car_state(s);
            }
        } else {
            let due: Vec<u64> = self
                .peers
                .iter()
                .filter(|p| {
                    p.car_pending
                        && p.car_passed_at
                            .is_none_or(|at| now - at >= CAR_STATE_PASS_MS)
                })
                .map(|p| p.serial)
                .collect();
            for s in due {
                self.pass_changed(s);
            }
        }
        self.tick_voice();
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, net::SocketAddr};

    use super::super::{
        Engine,
        testkit::{engine, seat},
    };
    use super::*;
    use crate::{
        fh2::carstate::{CarLook, MESSAGE_TYPE, encode_report, encode_report_with},
        stun,
    };

    const A: u64 = 1_000_000_000_000_201;
    const B: u64 = 1_000_000_000_000_202;
    const T0: i64 = 10_000;

    fn ep(index: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, index], 3074))
    }

    fn look(headlights: u8, roof_mode: u8) -> CarLook {
        CarLook {
            headlights,
            roof_mode,
            sequence: 1,
        }
    }

    fn glow(byte: u8) -> ModBlob {
        ModBlob::new(1, 1, &[byte; 40]).unwrap()
    }

    const OPEN_HOOD: Parts = Parts {
        signals: 1,
        parts: 0x0002,
    };

    /// A (index 1) and B (index 2) seated and synced with these signed sets (`None`: no claim),
    /// the refresh just done so only change passes go out.
    fn world(a: Option<&[u8]>, b: Option<&[u8]>) -> Engine {
        let mut e = engine();
        seat(&mut e, 1, A);
        seat(&mut e, 2, B);
        let mut sets = BTreeMap::new();
        if let Some(ids) = a {
            sets.insert(A, SyncSet::of(ids));
        }
        if let Some(ids) = b {
            sets.insert(B, SyncSet::of(ids));
        }
        e.set_sync_sets(&sets);
        e.now = T0;
        e.last_car_states = T0;
        e
    }

    /// B's hook says hello (a §5f one with a blob of its own, or a layout-1 one), at T0 - 500.
    fn b_hook(e: &mut Engine, speaks: bool) {
        let at = e.now;
        e.now = T0 - 500;
        let up = if speaks {
            encode_report_with(look(0, 0), B, None, &[glow(9)])
        } else {
            encode_report(look(0, 0), B)
        };
        e.receive_side_channel(&up, ep(2));
        e.now = at;
        e.take_out();
    }

    /// What reached B's hook: each datagram's attribute types.
    fn to_b(e: &mut Engine) -> Vec<Vec<u16>> {
        e.take_out()
            .into_iter()
            .filter(|s| s.to == ep(2))
            .map(|s| {
                assert!(s.data.len() <= stun::MAX_LEN);
                stun::attributes_of(&s.data, MESSAGE_TYPE)
                    .map(|(t, _)| t)
                    .collect()
            })
            .collect()
    }

    fn a_sends(e: &mut Engine, l: CarLook, parts: Option<Parts>, mods: &[ModBlob]) {
        e.receive_side_channel(&encode_report_with(l, A, parts, mods), ep(1));
    }

    const CORE: u16 = carstate::ATTRIBUTE_TYPE;
    const FC: u16 = carstate::PARTS_ATTRIBUTE;
    const FD: u16 = carstate::MOD_ATTRIBUTE;

    #[test]
    fn both_hold_both_ids_get_parts_and_the_blob() {
        let mut e = world(Some(&[1, 2]), Some(&[1, 2]));
        b_hook(&mut e, true);
        a_sends(&mut e, look(1, 0), Some(OPEN_HOOD), &[glow(7)]);
        let out = e.take_out();
        let d = &out.iter().find(|s| s.to == ep(2)).unwrap().data;
        let types: Vec<u16> = stun::attributes_of(d, MESSAGE_TYPE).map(|x| x.0).collect();
        assert_eq!(types, [CORE, FC, FD]);
        let ext = carstate::read_extensions(d, 0);
        assert_eq!(ext.parts, Some(OPEN_HOOD));
        assert_eq!(ext.mods().copied().collect::<Vec<_>>(), [glow(7)]);
        assert_eq!(
            carstate::read_peer_look(d).map(|p| (p.xuid, p.peer_index)),
            Some((A, 1))
        );
    }

    #[test]
    fn each_id_needs_both_sets() {
        // B lacks 2: the blob only
        let mut e = world(Some(&[1, 2]), Some(&[1]));
        b_hook(&mut e, true);
        a_sends(&mut e, look(1, 0), Some(OPEN_HOOD), &[glow(7)]);
        assert_eq!(to_b(&mut e), [vec![CORE, FD]]);
        // A lacks 1: its blob is never kept, the parts pass
        let mut e = world(Some(&[2]), Some(&[1, 2]));
        b_hook(&mut e, true);
        a_sends(&mut e, look(1, 0), Some(OPEN_HOOD), &[glow(7)]);
        assert_eq!(to_b(&mut e), [vec![CORE, FC]]);
        // disjoint sets: the core alone
        let mut e = world(Some(&[2]), Some(&[1]));
        b_hook(&mut e, true);
        a_sends(&mut e, look(1, 0), Some(OPEN_HOOD), &[glow(7)]);
        assert_eq!(to_b(&mut e), [vec![CORE]]);
    }

    #[test]
    fn neither_or_a_missing_claim_is_the_core_alone() {
        for (a, b) in [
            (Some(&[][..]), Some(&[][..])),
            (None, Some(&[1, 2][..])),
            (Some(&[1, 2][..]), None),
            (None, None),
        ] {
            let mut e = world(a, b);
            b_hook(&mut e, true);
            a_sends(&mut e, look(1, 0), Some(OPEN_HOOD), &[glow(7)]);
            assert_eq!(to_b(&mut e), [vec![CORE]], "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_layout_1_hook_gets_layout_1_byte_for_byte() {
        let mut e = world(Some(&[1, 2]), Some(&[1, 2]));
        b_hook(&mut e, false);
        a_sends(
            &mut e,
            look(1, 1),
            Some(Parts {
                signals: 0,
                parts: 1,
            }),
            &[glow(7)],
        );
        let out = e.take_out();
        let d = &out.iter().find(|s| s.to == ep(2)).unwrap().data;
        let p = carstate::read_peer_look(d).unwrap();
        let mut txid = [0u8; 12];
        txid.copy_from_slice(&d[8..20]);
        assert_eq!(*d, carstate::encode_peer_look_with(&p, &txid));
        assert_eq!(d.len(), 20 + 4 + 28);
    }

    #[test]
    fn a_new_hook_socket_must_speak_again() {
        let mut e = world(Some(&[1, 2]), Some(&[1, 2]));
        b_hook(&mut e, true);
        // B's §5f word came from another socket; its own socket now sends layout 1
        let other = SocketAddr::from(([192, 0, 2, 2], 3075));
        e.peers[1].car_hook = Some(other);
        e.now = T0 - 200;
        e.receive_side_channel(&encode_report(look(2, 0), B), ep(2));
        assert!(!e.peers[1].car_speaks);
        e.now = T0;
        e.take_out();
        a_sends(&mut e, look(1, 0), Some(OPEN_HOOD), &[glow(7)]);
        assert_eq!(to_b(&mut e), [vec![CORE]]);
    }

    #[test]
    fn invalid_extensions_leave_the_core() {
        let mut e = world(Some(&[1, 2]), Some(&[1, 2]));
        b_hook(&mut e, true);
        // bit 0 says the roof is open, the core says closed: the parts are refused
        a_sends(
            &mut e,
            look(1, 0),
            Some(Parts {
                signals: 0,
                parts: 1,
            }),
            &[],
        );
        assert_eq!(to_b(&mut e), [vec![CORE]]);
        e.now += 200;
        // a repeated id drops both blobs; the parts still pass
        a_sends(&mut e, look(2, 0), Some(OPEN_HOOD), &[glow(1), glow(2)]);
        assert_eq!(to_b(&mut e), [vec![CORE, FC]]);
    }

    #[test]
    fn changes_are_coalesced_to_ten_a_second_with_the_newest_state() {
        let mut e = world(Some(&[1, 2]), Some(&[1, 2]));
        b_hook(&mut e, true);
        a_sends(&mut e, look(1, 0), None, &[]);
        assert_eq!(to_b(&mut e).len(), 1);
        // inside the 50 ms accept floor: not even stored
        e.now = T0 + 20;
        a_sends(&mut e, look(2, 0), None, &[]);
        assert_eq!(e.peers[0].car_state.map(|s| s.headlights), Some(1));
        // stored, but the pass waits for the 100 ms window
        e.now = T0 + 60;
        a_sends(&mut e, look(2, 0), Some(OPEN_HOOD), &[]);
        assert!(to_b(&mut e).is_empty());
        e.now = T0 + 80;
        e.tick_side_channels();
        assert!(to_b(&mut e).is_empty());
        e.now = T0 + 100;
        e.tick_side_channels();
        let out = e.take_out();
        let d = &out.iter().find(|s| s.to == ep(2)).unwrap().data;
        assert_eq!(carstate::read_peer_look(d).unwrap().look.headlights, 2);
        assert_eq!(carstate::read_extensions(d, 0).parts, Some(OPEN_HOOD));
        e.now = T0 + 140;
        e.tick_side_channels();
        assert!(to_b(&mut e).is_empty(), "nothing pending any more");
        // at most ten change passes in a second, whatever the ups
        let mut passes = 0;
        for step in 1..=50 {
            e.now = T0 + 140 + step * 20;
            if step % 3 == 0 {
                let signals = (step % 4) as u8;
                a_sends(&mut e, look(1, 0), Some(Parts { signals, parts: 0 }), &[]);
            }
            e.tick_side_channels();
            passes += to_b(&mut e).len();
        }
        assert!(
            passes <= 10 + 1,
            "{passes} in one second (one refresh included)"
        );
    }

    #[test]
    fn changed_compares_parts_signals_and_blobs() {
        let mut e = world(Some(&[1, 2]), Some(&[1, 2]));
        b_hook(&mut e, true);
        a_sends(&mut e, look(1, 0), Some(OPEN_HOOD), &[glow(7)]);
        assert_eq!(to_b(&mut e).len(), 1);
        fn step(e: &mut Engine, parts: Option<Parts>, mods: &[ModBlob]) -> usize {
            e.now += 200;
            a_sends(e, look(1, 0), parts, mods);
            to_b(e).len()
        }
        assert_eq!(
            step(&mut e, Some(OPEN_HOOD), &[glow(7)]),
            0,
            "the same state"
        );
        assert_eq!(
            step(&mut e, Some(OPEN_HOOD), &[]),
            0,
            "the blob is kept 5 s"
        );
        let signals = Parts {
            signals: 2,
            ..OPEN_HOOD
        };
        assert_eq!(step(&mut e, Some(signals), &[]), 1, "signals");
        let trunk = Parts {
            parts: 0x0004,
            ..signals
        };
        assert_eq!(step(&mut e, Some(trunk), &[]), 1, "parts");
        assert_eq!(step(&mut e, Some(trunk), &[glow(8)]), 1, "a blob's bytes");
        assert_eq!(step(&mut e, None, &[glow(8)]), 1, "the parts gone");
    }

    #[test]
    fn a_blob_is_forgotten_five_seconds_after_it_was_last_sent() {
        let mut e = world(Some(&[1, 2]), Some(&[1, 2]));
        b_hook(&mut e, true);
        a_sends(&mut e, look(1, 0), None, &[glow(7)]);
        assert_eq!(to_b(&mut e), [vec![CORE, FD]]);
        e.now = T0 + 4000;
        a_sends(&mut e, look(1, 0), None, &[]);
        e.last_car_states = 0;
        e.tick_side_channels();
        assert_eq!(to_b(&mut e), [vec![CORE, FD]], "refresh at 4 s");
        e.now = T0 + 5000;
        e.last_car_states = 0;
        e.tick_side_channels();
        assert_eq!(to_b(&mut e), [vec![CORE]], "refresh at 5 s");
    }

    #[test]
    fn a_source_keeps_two_ids_at_most() {
        let mut e = world(Some(&[1, 3, 4]), Some(&[1, 3, 4]));
        b_hook(&mut e, true);
        let blob = |id| ModBlob::new(id, 1, &[id; 64]).unwrap();
        a_sends(&mut e, look(1, 0), None, &[blob(1), blob(3)]);
        e.now += 200;
        a_sends(&mut e, look(1, 0), None, &[blob(4)]);
        let out = e.take_out();
        let d = &out.iter().rev().find(|s| s.to == ep(2)).unwrap().data;
        let ids: Vec<u8> = carstate::read_extensions(d, 0)
            .mods()
            .map(|b| b.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&4));
        // the largest datagram §5f allows: 20 + 32 + 12 + 2 × 72
        let big = carstate::encode_peer_state_with(
            &carstate::read_peer_look(d).unwrap(),
            Some(OPEN_HOOD),
            &[blob(1), blob(3), blob(4)],
            &[0; 12],
        );
        assert_eq!(big.len(), 208);
    }
}
