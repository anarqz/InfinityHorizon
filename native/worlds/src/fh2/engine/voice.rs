// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Voice (OD-154, MATCHMAKING_API §5c, OD-164, OD-165): the world relay is the voice authority.
//! It never trusts a client for who speaks: a hook subscribes with its XUID (checked like car
//! state), its voice is then taken only from that socket and stamped with the sender's stable
//! voice seat (0..11). Every mute layer is enforced here, before fan-out: the host's mode (off,
//! open, proximity), the server's voice blocks (the policy's `voice_blocked`), the host's lobby
//! mutes, the talker's own mic mute, each listener's deafen and mute list, and the cap of 4
//! talkers. Nothing is recorded: Opus packets pass through, only counters remain.
//!
//! In proximity each (talker, listener) pair is decided here: same hearing group, same event
//! side, within R = 150 m with 15 % hysteresis, at most the 4 nearest per listener; the voice down
//! then carries both cars sampled at this instant. In a team event each player hears only its
//! hearing group (the team seams in [`super::seams`]), and state down is written per listener.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    net::SocketAddr,
};

use super::{Engine, dict::DotMap};
use crate::fh2::voice::{self as codec, Pose, PoseSide, Seat, VoiceMode, VoiceStatus, VoiceTag};

pub const VOICE_PACKETS_PER_SECOND: f64 = 20.0;
pub const VOICE_BYTES_PER_SECOND: f64 = 2048.0;
pub const MAX_TALKERS: usize = 4;
const TALKING_MS: i64 = 300;
const VOICE_STATE_MS: i64 = 1000;
/// R (§5c): the relay's, written in 0xC0F9; the host cannot choose it in v1.
pub const PROXIMITY_RANGE: u16 = 150;
/// The in-range bit clears past 1.15 R; a candidate replaces the farthest heard talker only
/// when 1.15 × nearer.
const PROXIMITY_HYSTERESIS: f64 = 1.15;
/// A car whose last own prediction is older than this is unknown.
const CAR_KNOWN_MS: i64 = 1500;
/// Velocity's provisional scale, m/s per raw unit (±32767 = ±200 m/s).
const CAR_VELOCITY_SCALE: f64 = 200.0 / 32767.0;
const JUMP_SLACK_M: f64 = 10.0;
const JUMP_MAX_INTERVAL_S: f64 = 1.5;
/// CarPredictionFull: velocity s16 × 3 at this bit, orientation s16 × 3 at the next.
const CAR_VELOCITY_BIT: usize = 96;
const CAR_ORIENTATION_BIT: usize = 192;

/// One subscribed hook.
#[derive(Debug, Clone)]
struct VoicePeer {
    hook: SocketAddr,
    self_flags: u8,
    mutes: HashSet<u64>,
    last_voice_at: Option<i64>,
    spurt_ended: bool,
    rate_at: i64,
    packets: f64,
    bytes: f64,
    /// Proximity: H(L), the talkers (serials) this listener hears now (at most 4).
    heard: Vec<u64>,
    /// Proximity: the talkers whose in-range bit with this listener is set.
    in_range: Vec<u64>,
}

/// An engine's voice state.
#[derive(Debug, Clone)]
pub(crate) struct Voice {
    /// The subscribers by serial, in the reference's map order (state downs and proximity
    /// downs draw their transaction ids in this order).
    peers: DotMap<u64, VoicePeer>,
    /// Stable voice seats 0..11 by serial: the lowest free when first needed, kept until the
    /// player leaves the engine.
    seats: DotMap<u64, u8>,
    muted_for_lobby: BTreeSet<u64>,
    state_due: DotMap<u64, ()>,
    mode: VoiceMode,
    last_state: i64,
    changed: bool,
    /// Why voice was dropped, by reason (counters only; proximity counts per pair).
    pub drops: BTreeMap<&'static str, u64>,
    pub forwarded: u64,
}

impl Default for Voice {
    fn default() -> Voice {
        Voice {
            peers: DotMap::new(),
            seats: DotMap::new(),
            muted_for_lobby: BTreeSet::new(),
            state_due: DotMap::new(),
            mode: VoiceMode::Open,
            last_state: 0,
            changed: false,
            drops: BTreeMap::new(),
            forwarded: 0,
        }
    }
}

/// A car at one instant.
#[derive(Debug, Clone, Copy, Default)]
struct CarSample {
    known: bool,
    age: u16,
    x: u32,
    y: u32,
    z: u32,
    vx: i16,
    vy: i16,
    vz: i16,
    qx: i16,
    qy: i16,
    qz: i16,
    jumps: u8,
}

impl CarSample {
    fn position(&self) -> [f32; 3] {
        [
            f32::from_bits(self.x),
            f32::from_bits(self.y),
            f32::from_bits(self.z),
        ]
    }

    fn side(&self) -> PoseSide {
        if self.known {
            PoseSide {
                car: true,
                age: self.age,
                x: self.x,
                y: self.y,
                z: self.z,
                vx: self.vx,
                vy: self.vy,
                vz: self.vz,
                jumps: self.jumps,
            }
        } else {
            PoseSide {
                jumps: self.jumps,
                ..PoseSide::default()
            }
        }
    }
}

/// Single-precision distance, as the reference computes it.
fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

impl Engine {
    // ---- launcher inputs ----

    /// The host's voice mode and the players it muted for everyone. Takes effect at once: the
    /// next voice packet follows it, a mode change clears the proximity state (range bits, heard
    /// sets), and every subscriber gets a state down now.
    pub fn set_voice(&mut self, mode: VoiceMode, lobby_muted: &[u64]) {
        let muted: BTreeSet<u64> = lobby_muted.iter().copied().collect();
        if self.voice.mode == mode && self.voice.muted_for_lobby == muted {
            return;
        }
        if self.voice.mode != mode {
            for (_, v) in self.voice.peers.iter_mut() {
                v.heard.clear();
                v.in_range.clear();
            }
        }
        self.voice.mode = mode;
        self.voice.muted_for_lobby = muted;
        self.send_voice_states();
    }

    /// The server's voice blocks changed (either source, §6.4): a talker blocked now leaves every
    /// heard set, and every subscriber learns the blocked bits at once.
    pub fn voice_blocks_changed(&mut self) {
        let mut blocked = Vec::new();
        for s in self.voice.peers.keys() {
            let Some(xuid) = self.peer(s).map(|p| p.xuid) else {
                continue;
            };
            if self.policy.voice_blocked(xuid, self.now) {
                blocked.push(s);
            }
        }
        for s in blocked {
            self.forget_heard(s, false);
        }
        self.send_voice_states();
    }

    /// The host's voice mode.
    pub fn voice_mode(&self) -> VoiceMode {
        self.voice.mode
    }

    /// Voice drops by reason and voice downs forwarded (counters only).
    pub fn voice_counters(&self) -> (&BTreeMap<&'static str, u64>, u64) {
        (&self.voice.drops, self.voice.forwarded)
    }

    /// The XUIDs heard talking now.
    pub fn voice_talking(&self) -> Vec<u64> {
        self.voice
            .peers
            .iter()
            .filter(|(_, v)| self.is_talking(v))
            .filter_map(|(s, _)| self.peer(*s).map(|p| p.xuid))
            .collect()
    }

    /// The players' hearing groups changed (a team event began or ended, a player left it or
    /// got its team): heard sets keep only their own group, and everyone's state changes at once.
    pub(super) fn voice_groups_changed(&mut self) {
        for listener in self.voice.peers.keys() {
            let heard = self
                .voice
                .peers
                .get(&listener)
                .map(|v| v.heard.clone())
                .unwrap_or_default();
            let keep: Vec<u64> = heard
                .into_iter()
                .filter(|t| self.same_hearing_group(*t, listener))
                .collect();
            if let Some(v) = self.voice.peers.get_mut(&listener) {
                v.heard = keep;
            }
        }
        self.send_voice_states();
    }

    // ---- the side channel ----

    fn drop_voice(&mut self, reason: &'static str) {
        *self.voice.drops.entry(reason).or_insert(0) += 1;
    }

    fn is_talking(&self, v: &VoicePeer) -> bool {
        v.last_voice_at.is_some_and(|at| self.now - at < TALKING_MS) && !v.spurt_ended
    }

    fn serial_talking(&self, serial: u64) -> bool {
        self.voice
            .peers
            .get(&serial)
            .is_some_and(|v| self.is_talking(v))
    }

    /// The synced player's voice seat (0..11); `None` only when all twelve are taken.
    fn voice_seat_of(&mut self, serial: u64) -> Option<u8> {
        if let Some(seat) = self.voice.seats.get(&serial) {
            return Some(*seat);
        }
        let taken: Vec<u8> = self.voice.seats.iter().map(|(_, s)| *s).collect();
        let seat = (0..codec::MAX_SEATS as u8).find(|s| !taken.contains(s))?;
        self.voice.seats.insert(serial, seat);
        Some(seat)
    }

    fn same_hearing_group(&self, a: u64, b: u64) -> bool {
        let group = self.hearing_group(a);
        group >= 0 && group == self.hearing_group(b)
    }

    pub(super) fn receive_voice_control(&mut self, data: &[u8], endpoint: SocketAddr) {
        let Some(control) = codec::parse_control(data) else {
            self.drop_voice("malformed");
            return;
        };
        let by_endpoint = self
            .peers
            .iter()
            .find(|p| p.endpoint == endpoint)
            .map(|p| (p.serial, p.xuid));
        if let Some((_, xuid)) = by_endpoint
            && xuid != control.xuid
        {
            self.drop_voice("spoofed");
            return;
        }
        let mut serial = by_endpoint.map(|x| x.0);
        if serial.is_none() {
            let candidates: Vec<u64> = self
                .peers
                .iter()
                .filter(|p| p.synced && p.xuid == control.xuid)
                .map(|p| p.serial)
                .collect();
            for c in candidates {
                if self.hook_source(c, endpoint) {
                    serial = Some(c);
                    break;
                }
            }
        }
        let authorized = serial.is_some_and(|s| {
            let Some(p) = self.peer(s) else { return false };
            let (synced, xuid, peer_endpoint) = (p.synced, p.xuid, p.endpoint);
            synced
                && self.allowed(xuid, peer_endpoint)
                && self.allowed_hook(xuid, endpoint)
                && self.take_side_rate(s, data.len(), true)
        });
        let Some(serial) = serial.filter(|_| authorized) else {
            self.drop_voice("unauthorized");
            return;
        };
        let now = self.now;
        if let Some(p) = self.peer_mut(serial) {
            p.last_seen = now;
        }
        if control.op == codec::LEAVE {
            if self.voice.peers.remove(&serial).is_some() {
                self.voice.changed = true;
                self.forget_heard(serial, false);
            }
            return;
        }
        if !self.voice.peers.contains_key(&serial) {
            if control.op != codec::SUBSCRIBE {
                self.drop_voice("not subscribed");
                return;
            }
            self.voice.peers.insert(
                serial,
                VoicePeer {
                    hook: endpoint,
                    self_flags: 0,
                    mutes: HashSet::new(),
                    last_voice_at: None,
                    spurt_ended: true,
                    rate_at: now,
                    packets: VOICE_PACKETS_PER_SECOND * 2.0,
                    bytes: VOICE_BYTES_PER_SECOND * 2.0,
                    heard: Vec::new(),
                    in_range: Vec::new(),
                },
            );
        }
        // a muted or deafened listener stops hearing at once
        let heard_xuids: Vec<(u64, Option<u64>)> = self
            .voice
            .peers
            .get(&serial)
            .map(|v| {
                v.heard
                    .iter()
                    .map(|t| (*t, self.peer(*t).map(|p| p.xuid)))
                    .collect()
            })
            .unwrap_or_default();
        let mut removed = false;
        if let Some(v) = self.voice.peers.get_mut(&serial) {
            v.hook = endpoint;
            v.self_flags = control.self_flags;
            v.mutes = control.mutes.iter().copied().collect();
            let deafened = v.self_flags & codec::DEAFENED != 0;
            let before = v.heard.len();
            v.heard.retain(|t| {
                let xuid = heard_xuids.iter().find(|(s, _)| s == t).and_then(|x| x.1);
                !(deafened || xuid.is_some_and(|x| v.mutes.contains(&x)))
            });
            removed = v.heard.len() != before;
        }
        if removed {
            self.voice.state_due.insert(serial, ());
        }
        self.voice.changed = true;
        if control.op == codec::SUBSCRIBE {
            self.send_voice_state(serial); // a newcomer learns the lobby at once
        }
        self.voice.state_due.remove(&serial);
        self.flush_voice_states();
    }

    pub(super) fn receive_voice(&mut self, data: &[u8], endpoint: SocketAddr) {
        let Some(up) = codec::read_voice(data) else {
            self.drop_voice("malformed");
            return;
        };
        let found = self
            .voice
            .peers
            .iter()
            .find(|(_, v)| v.hook == endpoint)
            .map(|(s, _)| *s);
        let bound = found.and_then(|s| self.peer(s).map(|p| (s, p.synced, p.xuid, p.endpoint)));
        let Some((serial, synced, xuid, peer_endpoint)) = bound else {
            self.drop_voice("unbound");
            return;
        };
        if !synced || !self.allowed(xuid, peer_endpoint) || !self.allowed_hook(xuid, endpoint) {
            self.drop_voice("unbound");
            return;
        }
        if !self.take_voice_rate(serial, data.len()) {
            self.drop_voice("rate");
            return;
        }
        // the rules, in order (§5c; 2a, the hearing groups, is applied per listener below)
        if self.voice.mode == VoiceMode::Off {
            self.drop_voice("lobby off");
            return;
        }
        let mic_muted = self
            .voice
            .peers
            .get(&serial)
            .is_some_and(|v| v.self_flags & codec::MIC_MUTED != 0);
        let refused = if self.policy.voice_blocked(xuid, self.now) {
            Some("blocked")
        } else if self.voice.muted_for_lobby.contains(&xuid) {
            Some("muted for lobby")
        } else if mic_muted {
            Some("mic muted")
        } else {
            None
        };
        if let Some(reason) = refused {
            self.drop_voice(reason);
            self.forget_heard(serial, false);
            self.flush_voice_states();
            return;
        }
        if self.voice.mode == VoiceMode::Proximity {
            self.receive_proximity(serial, &up);
            return;
        }
        // rule 6: the cap of 4 talking counts the talkers of the sender's hearing group
        if !self.serial_talking(serial) {
            let talking = self
                .voice
                .peers
                .iter()
                .filter(|(s, v)| {
                    **s != serial && self.is_talking(v) && self.same_hearing_group(**s, serial)
                })
                .count();
            if talking >= MAX_TALKERS {
                self.drop_voice("talker cap");
                return;
            }
        }
        let Some(seat) = self.voice_seat_of(serial) else {
            self.drop_voice("no seat");
            return;
        };
        self.start_packet(serial, up.tag);
        let packet = codec::write_down(up.tag, seat, &up.opus, None, self.voice_tail());
        for listener in self.voice.peers.keys() {
            if listener == serial || !self.listening(listener, serial) {
                continue;
            }
            if !self.same_hearing_group(serial, listener) {
                self.drop_voice("other group");
                continue;
            }
            if let Some(hook) = self.voice.peers.get(&listener).map(|v| v.hook) {
                self.emit(hook, packet.clone());
                self.voice.forwarded += 1;
            }
        }
    }

    /// A voice down's random tail (the 3 bytes after its seat).
    fn voice_tail(&mut self) -> [u8; 3] {
        let mut tail = [0u8; 3];
        self.random.fill(&mut tail);
        tail
    }

    /// Rule 7's per-listener exclusions: subscribed, synced and allowed, not deafened, not
    /// muting the talker.
    fn listening(&mut self, listener: u64, talker: u64) -> bool {
        let Some(v) = self.voice.peers.get(&listener) else {
            return false;
        };
        let (hook, self_flags) = (v.hook, v.self_flags);
        let Some(p) = self.peer(listener) else {
            return false;
        };
        let (synced, xuid, endpoint) = (p.synced, p.xuid, p.endpoint);
        let talker_xuid = self.peer(talker).map(|p| p.xuid);
        synced
            && self.allowed(xuid, endpoint)
            && self.allowed_hook(xuid, hook)
            && self_flags & codec::DEAFENED == 0
            && !self
                .voice
                .peers
                .get(&listener)
                .is_some_and(|v| talker_xuid.is_some_and(|x| v.mutes.contains(&x)))
    }

    fn start_packet(&mut self, serial: u64, header: VoiceTag) {
        let started = !self.serial_talking(serial);
        let now = self.now;
        let Some(v) = self.voice.peers.get_mut(&serial) else {
            return;
        };
        v.last_voice_at = Some(now);
        v.spurt_ended = header.marks & codec::END_OF_SPURT != 0;
        if started || v.spurt_ended {
            self.voice.changed = true;
        }
    }

    /// §5c "The relay's rules in proximity": per listener, side, range with hysteresis, the 4
    /// nearest, then the pose.
    fn receive_proximity(&mut self, serial: u64, up: &codec::VoicePacket) {
        let Some(seat) = self.voice_seat_of(serial) else {
            self.drop_voice("no seat");
            return;
        };
        self.start_packet(serial, up.tag);
        let talker = self.sample_car(serial);
        let range = f64::from(PROXIMITY_RANGE);
        for listener in self.voice.peers.keys() {
            if listener == serial {
                continue;
            }
            if !self.listening(listener, serial) {
                self.unhear(listener, serial);
                continue;
            }
            if !self.same_hearing_group(serial, listener) {
                self.unhear(listener, serial);
                self.drop_voice("other group");
                continue;
            }
            if !self.same_side(serial, listener) {
                self.unhear(listener, serial);
                self.drop_voice("other side");
                continue;
            }
            let me = self.sample_car(listener);
            let mine = if talker.known && me.known {
                let d = f64::from(distance(talker.position(), me.position()));
                let Some(v) = self.voice.peers.get_mut(&listener) else {
                    continue;
                };
                if d <= range {
                    if !v.in_range.contains(&serial) {
                        v.in_range.push(serial);
                    }
                } else if d > range * PROXIMITY_HYSTERESIS {
                    v.in_range.retain(|t| *t != serial);
                }
                if !v.in_range.contains(&serial) {
                    self.unhear(listener, serial);
                    self.drop_voice("out of range");
                    continue;
                }
                Some(d)
            } else {
                if let Some(v) = self.voice.peers.get_mut(&listener) {
                    v.in_range.retain(|t| *t != serial); // cleared while either car is unknown
                }
                // a carless talker ranks last; a carless listener has no distances
                me.known.then_some(range)
            };
            let heard = self
                .voice
                .peers
                .get(&listener)
                .map(|v| v.heard.clone())
                .unwrap_or_default();
            if !heard.contains(&serial) {
                if heard.len() >= MAX_TALKERS {
                    let far = self.farthest(&heard, &me);
                    let replace = match (mine, far) {
                        (Some(m), Some((_, fd))) => m * PROXIMITY_HYSTERESIS < fd,
                        _ => false,
                    };
                    if !replace {
                        self.drop_voice("listener cap");
                        continue;
                    }
                    if let (Some((fp, _)), Some(v)) = (far, self.voice.peers.get_mut(&listener)) {
                        v.heard.retain(|t| *t != fp);
                    }
                }
                if let Some(v) = self.voice.peers.get_mut(&listener) {
                    v.heard.push(serial);
                }
                self.voice.state_due.insert(listener, ());
            }
            let pose = Pose {
                talker: talker.side(),
                listener: me.side(),
                qx: if me.known { me.qx } else { 0 },
                qy: if me.known { me.qy } else { 0 },
                qz: if me.known { me.qz } else { 0 },
                same_side: true,
            };
            let Some(hook) = self.voice.peers.get(&listener).map(|v| v.hook) else {
                continue;
            };
            let tail = self.voice_tail();
            let packet = codec::write_down(up.tag, seat, &up.opus, Some(&pose), tail);
            self.emit(hook, packet);
            self.voice.forwarded += 1;
        }
        if self.voice.peers.get(&serial).is_some_and(|v| v.spurt_ended) {
            self.forget_heard(serial, false); // its spurt ended: it leaves every heard set
        }
        self.flush_voice_states();
    }

    /// F: the farthest member of H(L) (a member whose car is unknown ranks R); among equally far,
    /// the higher seat.
    fn farthest(&self, heard: &[u64], me: &CarSample) -> Option<(u64, f64)> {
        let mut far: Option<(u64, f64, i32)> = None;
        for &member in heard {
            let sample = self.sample_car(member);
            let d = if sample.known {
                f64::from(distance(sample.position(), me.position()))
            } else {
                f64::from(PROXIMITY_RANGE)
            };
            let seat = self.voice.seats.get(&member).map_or(-1, |s| i32::from(*s));
            if far.is_none_or(|(_, fd, fs)| d > fd || d == fd && seat > fs) {
                far = Some((member, d, seat));
            }
        }
        far.map(|(p, d, _)| (p, d))
    }

    fn unhear(&mut self, listener: u64, talker: u64) {
        let Some(v) = self.voice.peers.get_mut(&listener) else {
            return;
        };
        if v.heard.contains(&talker) {
            v.heard.retain(|t| *t != talker);
            self.voice.state_due.insert(listener, ());
        }
    }

    /// The talker leaves every heard set (its spurt ended, it left, or a rule now refuses it);
    /// with `ranges` its range bits go too.
    fn forget_heard(&mut self, talker: u64, ranges: bool) {
        for listener in self.voice.peers.keys() {
            self.unhear(listener, talker);
            if ranges && let Some(v) = self.voice.peers.get_mut(&listener) {
                v.in_range.retain(|t| *t != talker);
            }
        }
    }

    /// A car prediction of the player's own car was accepted (before its position and car id
    /// move): the jump counter counts a move the last velocity does not explain (|Δp| > |v_old| ×
    /// Δt + 10 m, Δt capped at 1.5 s), a new car handle, or a car known again after being unknown.
    pub(super) fn car_accepted(&mut self, serial: u64, car_id: u32, position: [f32; 3]) {
        let now = self.now;
        let Some(p) = self.peer_mut(serial) else {
            return;
        };
        let Some(at) = p.last_car_at else { return };
        let mut jumped = now - at > CAR_KNOWN_MS || p.car_id != Some(car_id);
        if !jumped && let (Some(old), Some(body)) = (p.car_position, &p.last_car_body) {
            let mut r = body.reader();
            let v = if r.skip(CAR_VELOCITY_BIT).is_ok() {
                let mut c = || r.read(16).map_or(0, |x| x as u16 as i16);
                let scale = CAR_VELOCITY_SCALE as f32;
                let (vx, vy, vz) = (c(), c(), c());
                [
                    f32::from(vx) * scale,
                    f32::from(vy) * scale,
                    f32::from(vz) * scale,
                ]
            } else {
                [0.0; 3]
            };
            let speed = distance(v, [0.0; 3]);
            let dt = JUMP_MAX_INTERVAL_S.min((now - at).max(0) as f64 / 1000.0);
            jumped = f64::from(distance(position, old)) > f64::from(speed) * dt + JUMP_SLACK_M;
        }
        if jumped {
            p.car_jumps = p.car_jumps.wrapping_add(1);
        }
    }

    /// The player's car at this instant: known when its last own prediction is at most 1500 ms
    /// old (a car sent again with a newer time never refreshes it). Position bits are copied as
    /// received, velocity and orientation raw.
    fn sample_car(&self, serial: u64) -> CarSample {
        let Some(p) = self.peer(serial) else {
            return CarSample::default();
        };
        let unknown = CarSample {
            jumps: p.car_jumps,
            ..CarSample::default()
        };
        let (Some(at), Some(body)) = (p.last_car_at, &p.last_car_body) else {
            return unknown;
        };
        if self.now - at > CAR_KNOWN_MS || body.len() != super::message::CAR_BODY_BITS {
            return unknown;
        }
        let mut r = body.reader();
        let mut read = |w: u32| r.read(w).unwrap_or(0);
        let (x, y, z) = (read(32) as u32, read(32) as u32, read(32) as u32);
        let (vx, vy, vz) = (
            read(16) as u16 as i16,
            read(16) as u16 as i16,
            read(16) as u16 as i16,
        );
        // skip to the orientation
        for _ in 0..(CAR_ORIENTATION_BIT - CAR_VELOCITY_BIT - 48) / 16 {
            read(16);
        }
        let (qx, qy, qz) = (
            read(16) as u16 as i16,
            read(16) as u16 as i16,
            read(16) as u16 as i16,
        );
        CarSample {
            known: true,
            age: (self.now - at).clamp(0, i64::from(u16::MAX)) as u16,
            x,
            y,
            z,
            vx,
            vy,
            vz,
            qx,
            qy,
            qz,
            jumps: p.car_jumps,
        }
    }

    fn take_voice_rate(&mut self, serial: u64, bytes: usize) -> bool {
        let now = self.now;
        let Some(v) = self.voice.peers.get_mut(&serial) else {
            return false;
        };
        let elapsed = (now - v.rate_at).max(0) as f64 / 1000.0;
        v.rate_at = now;
        v.packets =
            (VOICE_PACKETS_PER_SECOND * 2.0).min(v.packets + elapsed * VOICE_PACKETS_PER_SECOND);
        v.bytes = (VOICE_BYTES_PER_SECOND * 2.0).min(v.bytes + elapsed * VOICE_BYTES_PER_SECOND);
        if v.packets < 1.0 || v.bytes < bytes as f64 {
            return false;
        }
        v.packets -= 1.0;
        v.bytes -= bytes as f64;
        true
    }

    /// State down is written per listener in proximity and during a team event.
    fn voice_state_per_listener(&self) -> bool {
        self.voice.mode == VoiceMode::Proximity || self.team_event_active()
    }

    fn voice_state_for(&mut self, listener: Option<u64>) -> VoiceStatus {
        let synced: Vec<(u64, u64)> = self
            .peers
            .iter()
            .filter(|p| p.synced)
            .map(|p| (p.serial, p.xuid))
            .collect();
        let heard = listener
            .and_then(|l| self.voice.peers.get(&l))
            .map(|v| v.heard.clone());
        let mut seats = Vec::new();
        for (s, xuid) in synced {
            let Some(seat) = self.voice_seat_of(s) else {
                continue;
            };
            let mut state = 0u8;
            let mut talking = self.serial_talking(s);
            // per listener: only the talkers it hears (its heard set in proximity, its hearing
            // group in open); its own seat as is
            if talking && let Some(l) = listener.filter(|l| *l != s) {
                talking = if self.voice.mode == VoiceMode::Proximity {
                    heard.as_ref().is_some_and(|h| h.contains(&s))
                } else {
                    self.same_hearing_group(s, l)
                };
            }
            if talking {
                state |= codec::TALKING;
            }
            if self.voice.muted_for_lobby.contains(&xuid) {
                state |= codec::MUTED_FOR_LOBBY;
            }
            if self.policy.voice_blocked(xuid, self.now) {
                state |= codec::BLOCKED;
            }
            seats.push(Seat { seat, xuid, state });
        }
        let mode = self.voice.mode;
        VoiceStatus {
            lobby: u8::from(mode != VoiceMode::Off),
            bundle: 3,
            seats,
            mode,
            range: if mode == VoiceMode::Proximity {
                PROXIMITY_RANGE
            } else {
                0
            },
            team: listener.and_then(|l| self.team_of(l)),
        }
    }

    fn send_voice_state(&mut self, serial: u64) {
        let Some(hook) = self.voice.peers.get(&serial).map(|v| v.hook) else {
            return;
        };
        let Some(p) = self.peer(serial) else { return };
        let (synced, xuid, endpoint) = (p.synced, p.xuid, p.endpoint);
        if synced && self.allowed(xuid, endpoint) && self.allowed_hook(xuid, hook) {
            let listener = self.voice_state_per_listener().then_some(serial);
            let state = self.voice_state_for(listener);
            let txid = self.random.transaction();
            self.emit(hook, codec::write_state(&state, &txid));
        }
    }

    /// Every subscriber's state, now.
    fn send_voice_states(&mut self) {
        self.voice.changed = false;
        self.voice.state_due.clear();
        self.voice.last_state = self.now;
        for s in self.voice.peers.keys() {
            self.send_voice_state(s);
        }
    }

    /// A listener whose heard set changed gets its state at once (proximity, team events).
    fn flush_voice_states(&mut self) {
        if self.voice.state_due.is_empty() {
            return;
        }
        for s in self.voice.state_due.keys() {
            if self.voice.peers.contains_key(&s) {
                self.send_voice_state(s);
            }
        }
        self.voice.state_due.clear();
    }

    pub(super) fn tick_voice(&mut self) {
        // a spurt that stopped without its last packet ends here, so "talking" never sticks
        let now = self.now;
        for s in self.voice.peers.keys() {
            let Some(v) = self.voice.peers.get_mut(&s) else {
                continue;
            };
            if !v.spurt_ended && v.last_voice_at.is_some_and(|at| now - at >= TALKING_MS) {
                v.spurt_ended = true;
                self.voice.changed = true;
                self.forget_heard(s, false);
            }
        }
        if !self.voice.changed && now - self.voice.last_state < VOICE_STATE_MS {
            self.flush_voice_states();
            return;
        }
        self.send_voice_states();
    }

    /// The player left the engine: its subscription and its seat go.
    pub(super) fn voice_peer_left(&mut self, serial: u64) {
        let subscribed = self.voice.peers.remove(&serial).is_some();
        let seated = self.voice.seats.remove(&serial).is_some();
        self.voice.changed |= subscribed || seated;
        self.forget_heard(serial, true);
    }
}
