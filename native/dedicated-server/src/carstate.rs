// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! What ExtraOptions changes on a player's own car and the game never sends: its headlights
//! (forced on or off, or the game's automatic) and its roof (open or closed). The hooks tell
//! the relay with a private STUN indication on the socket they play from (the joining
//! player's proxy; the host's hook from a loopback socket of its own), and the relay passes each
//! player's state to the others with the sender's index, XUID and the position of its car (from
//! its last CarPredictionFull), by which their hooks find that car among theirs.
//!
//! A STUN message (RFC 5389 header, magic cookie) of a type no STUN server or client uses
//! (indication class, method 0xE91), so the relay's STUN code, the joining player's proxy and
//! older hooks drop it without effect, and it never reaches the game's NerdCore parser. One
//! comprehension-optional attribute (0xC0F1) carries the state:
//!   player -> relay (16 bytes): version, lights, roof, 0, seq (u16), 0 (u16), the player's XUID (u64)
//!   relay -> players (28 bytes): version, lights, roof, sender index, seq (u16), flags (bit 0: a
//!     position follows), 0, the sender's XUID (u64), its car's x, y, z (f32)
//! All big-endian, as STUN.

use crate::stun;

/// The message type: indication class (C1 = 0, C0 = 1), method 0xE91.
pub const KIND: u16 = 0x3E91;
pub const ATTR: u16 = 0xC0F1;
pub const VERSION: u8 = 1;
const UP_LEN: usize = 16;
const DOWN_LEN: usize = 28;

/// Headlights: the game's own (automatic), forced on, forced off.
pub const LIGHTS_GAME: u8 = 0;
pub const LIGHTS_ON: u8 = 1;
pub const LIGHTS_OFF: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    pub lights: u8,
    pub roof: u8,
    pub seq: u16,
}

/// A player's indication: its state and the XUID it says it plays as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Up {
    pub state: State,
    pub xuid: u64,
}

/// What the relay passes on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Down {
    pub state: State,
    pub index: u8,
    pub xuid: u64,
    pub position: Option<[f32; 3]>,
}

/// Whether `data` is one of these (any direction); its body is checked by `parse_up`.
pub fn is_carstate(data: &[u8]) -> bool {
    stun::is_stun(data) && u16::from_be_bytes([data[0], data[1]]) == KIND
}

fn attribute(data: &[u8]) -> Option<&[u8]> {
    if !is_carstate(data) {
        return None;
    }
    let mut at = 20;
    while at + 4 <= data.len() {
        let t = u16::from_be_bytes([data[at], data[at + 1]]);
        let n = u16::from_be_bytes([data[at + 2], data[at + 3]]) as usize;
        let value = data.get(at + 4..at + 4 + n)?;
        if t == ATTR {
            return Some(value);
        }
        at += 4 + n.div_ceil(4) * 4;
    }
    None
}

fn state_ok(s: &State) -> bool {
    s.lights <= LIGHTS_OFF && s.roof <= 1
}

/// A player's indication, or None (another size, version or out-of-range values).
pub fn parse_up(data: &[u8]) -> Option<Up> {
    let v = attribute(data)?;
    if v.len() != UP_LEN || v[0] != VERSION {
        return None;
    }
    let state = State {
        lights: v[1],
        roof: v[2],
        seq: u16::from_be_bytes([v[4], v[5]]),
    };
    let xuid = u64::from_be_bytes(v[8..16].try_into().ok()?);
    state_ok(&state).then_some(Up { state, xuid })
}

pub fn parse_down(data: &[u8]) -> Option<Down> {
    let v = attribute(data)?;
    if v.len() != DOWN_LEN || v[0] != VERSION {
        return None;
    }
    let state = State {
        lights: v[1],
        roof: v[2],
        seq: u16::from_be_bytes([v[4], v[5]]),
    };
    let xuid = u64::from_be_bytes(v[8..16].try_into().ok()?);
    let f = |i: usize| f32::from_be_bytes(v[i..i + 4].try_into().unwrap());
    let position = (v[6] & 1 != 0).then(|| [f(16), f(20), f(24)]);
    state_ok(&state).then_some(Down {
        state,
        index: v[3],
        xuid,
        position,
    })
}

fn message(value: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(24 + value.len());
    m.extend_from_slice(&KIND.to_be_bytes());
    m.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
    m.extend_from_slice(&stun::MAGIC.to_be_bytes());
    m.extend_from_slice(&stun::new_txid());
    m.extend_from_slice(&ATTR.to_be_bytes());
    m.extend_from_slice(&(value.len() as u16).to_be_bytes());
    m.extend_from_slice(value);
    m
}

pub fn up(state: State, xuid: u64) -> Vec<u8> {
    let mut v = [0u8; UP_LEN];
    v[0] = VERSION;
    v[1] = state.lights;
    v[2] = state.roof;
    v[4..6].copy_from_slice(&state.seq.to_be_bytes());
    v[8..16].copy_from_slice(&xuid.to_be_bytes());
    message(&v)
}

pub fn down(d: &Down) -> Vec<u8> {
    let mut v = [0u8; DOWN_LEN];
    v[0] = VERSION;
    v[1] = d.state.lights;
    v[2] = d.state.roof;
    v[3] = d.index;
    v[4..6].copy_from_slice(&d.state.seq.to_be_bytes());
    v[8..16].copy_from_slice(&d.xuid.to_be_bytes());
    if let Some(p) = d.position {
        v[6] = 1;
        for (i, c) in p.iter().enumerate() {
            v[16 + 4 * i..20 + 4 * i].copy_from_slice(&c.to_be_bytes());
        }
    }
    message(&v)
}
