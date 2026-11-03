// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The world relay's voice side channel (OD-154, MATCHMAKING_API §5c, contract §5.3): STUN
//! indications like car state, big-endian, on the world socket.
//!
//! - Voice, kind 0x3E93: the 12-byte transaction id is {ver 1, flags, seq u16, ts u32 (20 ms
//!   units), up: 4 random | down: seat u8, 3 random}; the attribute (0xC0F3 up, 0xC0F4 down) is
//!   one Opus packet (1-400 bytes). In proximity a voice down also carries the pose 0xC0F8.
//! - Control and state, kind 0x3E94: control up 0xC0F5 {ver, op, xuid u64, self u8, count u8 ≤
//!   12, count × xuid u64}; state down 0xC0F6 {ver, lobby u8, bundle u8, count u8 ≤ 12, count ×
//!   {seat u8, xuid u64, state u8}}, then the mode 0xC0F9 {ver, mode, range u16} and, for a
//!   running team event's participant only, the team 0xC0FA {ver, flags (bit 0 running), team,
//!   0}.
//!
//! A writer puts the message's attribute first; a reader finds it anywhere, and the whole
//! datagram must walk (every attribute within it).

use crate::stun;

pub const VOICE_KIND: u16 = 0x3E93;
pub const CONTROL_KIND: u16 = 0x3E94;
const VOICE_UP: u16 = 0xC0F3;
const VOICE_DOWN: u16 = 0xC0F4;
const CONTROL_UP: u16 = 0xC0F5;
const STATE_DOWN: u16 = 0xC0F6;
const POSE: u16 = 0xC0F8;
const MODE: u16 = 0xC0F9;
const TEAM: u16 = 0xC0FA;
pub const VERSION: u8 = 1;
pub const MAX_OPUS: usize = 400;
pub const MAX_SEATS: usize = 12;
const POSE_LEN: usize = 48;

/// A voice header's flags: the last packet of a talk spurt.
pub const END_OF_SPURT: u8 = 1;
/// A voice header's flags: sent while push-to-talk is held.
pub const PUSH_TO_TALK: u8 = 2;

/// Control ops.
pub const SUBSCRIBE: u8 = 1;
pub const REPLACE_MUTES: u8 = 2;
pub const LEAVE: u8 = 3;
/// Control's `self` bits.
pub const MIC_MUTED: u8 = 1;
pub const DEAFENED: u8 = 2;

/// A state seat's bits.
pub const TALKING: u8 = 1;
pub const MUTED_FOR_LOBBY: u8 = 2;
pub const BLOCKED: u8 = 4;

/// The lobby's voice mode (§5c "The mode", CR-L46): the host's, enforced by its relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoiceMode {
    Off,
    #[default]
    Open,
    Proximity,
}

impl VoiceMode {
    /// The control channel's strict reading (`world.voice`: anything else is `bad_request`).
    pub fn parse(s: &str) -> Option<VoiceMode> {
        match s {
            "off" => Some(VoiceMode::Off),
            "open" => Some(VoiceMode::Open),
            "proximity" => Some(VoiceMode::Proximity),
            _ => None,
        }
    }

    /// The engine-level reading: an unknown mode is off (MACBOOK's decision; the reference
    /// engine refuses it).
    pub fn or_off(s: &str) -> VoiceMode {
        VoiceMode::parse(s).unwrap_or(VoiceMode::Off)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            VoiceMode::Off => "off",
            VoiceMode::Open => "open",
            VoiceMode::Proximity => "proximity",
        }
    }

    pub fn byte(self) -> u8 {
        match self {
            VoiceMode::Off => 0,
            VoiceMode::Open => 1,
            VoiceMode::Proximity => 2,
        }
    }
}

/// A voice datagram's header (the transaction id's first 8 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    pub seq: u16,
    pub ts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Up {
    pub header: Header,
    pub opus: Vec<u8>,
}

/// A hook's voice control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub op: u8,
    pub xuid: u64,
    pub self_flags: u8,
    pub mutes: Vec<u64>,
}

/// One side of a pose: a car the relay knows, the age of its sample (ms), its position as the
/// f32 bit patterns it came with, its raw velocity and the low bits of the jump counter (which
/// rides in the flags whatever the car bit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoseSide {
    pub car: bool,
    pub age: u16,
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub vx: i16,
    pub vy: i16,
    pub vz: i16,
    pub jumps: u8,
}

/// The pose of a proximity voice down: both cars at one relay instant and the listener's
/// orientation (x, y, z of a unit quaternion, raw; zero without a listener car).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pose {
    pub talker: PoseSide,
    pub listener: PoseSide,
    pub qx: i16,
    pub qy: i16,
    pub qz: i16,
    pub same_side: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Seat {
    pub seat: u8,
    pub xuid: u64,
    pub state: u8,
}

/// The relay's voice state for one subscriber: `lobby` bit 0 voice on, `bundle` 3 or 6, the
/// seats; the mode with its range (0xC0F9); the team (0xC0FA) for a team event's participant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub lobby: u8,
    pub bundle: u8,
    pub seats: Vec<Seat>,
    pub mode: VoiceMode,
    pub range: u16,
    pub team: Option<u8>,
}

fn be16(d: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([d[at], d[at + 1]])
}

fn be64(d: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[at..at + 8]);
    u64::from_be_bytes(b)
}

/// A well-formed STUN datagram of `kind` whose every attribute lies within it, and the value of
/// its first `attribute`.
fn attribute(data: &[u8], kind: u16, attribute: u16) -> Option<&[u8]> {
    if !stun::is_stun(data) || be16(data, 0) != kind {
        return None;
    }
    let mut found = None;
    let mut at = 20;
    while at + 4 <= data.len() {
        let tag = be16(data, at);
        let n = be16(data, at + 2) as usize;
        let value = data.get(at + 4..at + 4 + n)?;
        if tag == attribute && found.is_none() {
            found = Some(value);
        }
        at += 4 + n.div_ceil(4) * 4;
    }
    found
}

fn header(data: &[u8]) -> Header {
    Header {
        flags: data[9],
        seq: be16(data, 10),
        ts: u32::from_be_bytes([data[12], data[13], data[14], data[15]]),
    }
}

/// An indication: the attributes in this order, each padded to 4 bytes.
fn indication(kind: u16, txid: &[u8; 12], attributes: &[(u16, &[u8])]) -> Vec<u8> {
    let body: usize = attributes
        .iter()
        .map(|(_, v)| 4 + v.len().div_ceil(4) * 4)
        .sum();
    let mut m = Vec::with_capacity(20 + body);
    m.extend_from_slice(&kind.to_be_bytes());
    m.extend_from_slice(&(body as u16).to_be_bytes());
    m.extend_from_slice(&stun::MAGIC.to_be_bytes());
    m.extend_from_slice(txid);
    for (tag, value) in attributes {
        m.extend_from_slice(&tag.to_be_bytes());
        m.extend_from_slice(&(value.len() as u16).to_be_bytes());
        m.extend_from_slice(value);
        m.resize(m.len().next_multiple_of(4), 0);
    }
    m
}

pub fn parse_up(data: &[u8]) -> Option<Up> {
    let opus = attribute(data, VOICE_KIND, VOICE_UP)?;
    if opus.is_empty() || opus.len() > MAX_OPUS || data[8] != VERSION {
        return None;
    }
    Some(Up {
        header: header(data),
        opus: opus.to_vec(),
    })
}

pub fn parse_control(data: &[u8]) -> Option<Control> {
    let v = attribute(data, CONTROL_KIND, CONTROL_UP)?;
    if v.len() < 12 || v[0] != VERSION {
        return None;
    }
    let (op, self_flags, count) = (v[1], v[10], v[11] as usize);
    if !(SUBSCRIBE..=LEAVE).contains(&op)
        || self_flags > 3
        || count > MAX_SEATS
        || v.len() != 12 + count * 8
    {
        return None;
    }
    Some(Control {
        op,
        xuid: be64(v, 2),
        self_flags,
        mutes: (0..count).map(|i| be64(v, 12 + i * 8)).collect(),
    })
}

/// A hook's voice packet (tests and tools): `random` is the transaction id's 4-byte tail.
pub fn write_up(up: &Up, random: [u8; 4]) -> Option<Vec<u8>> {
    if up.opus.is_empty() || up.opus.len() > MAX_OPUS {
        return None;
    }
    let mut t = voice_txid(up.header);
    t[8..].copy_from_slice(&random);
    Some(indication(VOICE_KIND, &t, &[(VOICE_UP, &up.opus)]))
}

/// A hook's control (tests and tools).
pub fn write_control(c: &Control, txid: &[u8; 12]) -> Option<Vec<u8>> {
    if c.mutes.len() > MAX_SEATS || !(SUBSCRIBE..=LEAVE).contains(&c.op) || c.self_flags > 3 {
        return None;
    }
    let mut v = vec![VERSION, c.op];
    v.extend_from_slice(&c.xuid.to_be_bytes());
    v.push(c.self_flags);
    v.push(c.mutes.len() as u8);
    for m in &c.mutes {
        v.extend_from_slice(&m.to_be_bytes());
    }
    Some(indication(CONTROL_KIND, txid, &[(CONTROL_UP, &v)]))
}

fn voice_txid(h: Header) -> [u8; 12] {
    let mut t = [0u8; 12];
    t[0] = VERSION;
    t[1] = h.flags;
    t[2..4].copy_from_slice(&h.seq.to_be_bytes());
    t[4..8].copy_from_slice(&h.ts.to_be_bytes());
    t
}

/// A voice down: the talker's header and Opus stamped with its seat; `tail` is the transaction
/// id's 3 random bytes after the seat.
pub fn write_down(
    header: Header,
    seat: u8,
    opus: &[u8],
    pose: Option<&Pose>,
    tail: [u8; 3],
) -> Vec<u8> {
    let mut t = voice_txid(header);
    t[8] = seat;
    t[9..].copy_from_slice(&tail);
    match pose {
        None => indication(VOICE_KIND, &t, &[(VOICE_DOWN, opus)]),
        Some(p) => {
            let pose = pose_value(p);
            indication(VOICE_KIND, &t, &[(VOICE_DOWN, opus), (POSE, &pose)])
        }
    }
}

fn pose_value(p: &Pose) -> [u8; POSE_LEN] {
    let mut v = [0u8; POSE_LEN];
    v[0] = VERSION;
    v[1] = u8::from(p.talker.car)
        | u8::from(p.listener.car) << 1
        | u8::from(p.same_side) << 2
        | (p.talker.jumps & 3) << 4
        | (p.listener.jumps & 3) << 6;
    side_value(&mut v[2..22], &p.talker);
    side_value(&mut v[22..42], &p.listener);
    if p.listener.car {
        v[42..44].copy_from_slice(&p.qx.to_be_bytes());
        v[44..46].copy_from_slice(&p.qy.to_be_bytes());
        v[46..48].copy_from_slice(&p.qz.to_be_bytes());
    }
    v
}

fn side_value(v: &mut [u8], s: &PoseSide) {
    if !s.car {
        return; // all zero
    }
    v[0..2].copy_from_slice(&s.age.to_be_bytes());
    v[2..6].copy_from_slice(&s.x.to_be_bytes());
    v[6..10].copy_from_slice(&s.y.to_be_bytes());
    v[10..14].copy_from_slice(&s.z.to_be_bytes());
    v[14..16].copy_from_slice(&s.vx.to_be_bytes());
    v[16..18].copy_from_slice(&s.vy.to_be_bytes());
    v[18..20].copy_from_slice(&s.vz.to_be_bytes());
}

/// A state down. At most 12 seats (the engine never has more).
pub fn write_state(s: &State, txid: &[u8; 12]) -> Vec<u8> {
    let seats = &s.seats[..s.seats.len().min(MAX_SEATS)];
    let mut v = vec![VERSION, s.lobby, s.bundle, seats.len() as u8];
    for seat in seats {
        v.push(seat.seat);
        v.extend_from_slice(&seat.xuid.to_be_bytes());
        v.push(seat.state);
    }
    let mut mode = vec![VERSION, s.mode.byte()];
    mode.extend_from_slice(&s.range.to_be_bytes());
    match s.team {
        None => indication(CONTROL_KIND, txid, &[(STATE_DOWN, &v), (MODE, &mode)]),
        Some(team) => {
            let team = [VERSION, 1, team, 0];
            indication(
                CONTROL_KIND,
                txid,
                &[(STATE_DOWN, &v), (MODE, &mode), (TEAM, &team)],
            )
        }
    }
}
