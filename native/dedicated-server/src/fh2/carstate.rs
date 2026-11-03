// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The car-state side channel (contract §5.3, MATCHMAKING_API §5a): a hook tells the world its
//! car's forced headlights and roof ("up"), and the world passes each player's state to the other
//! hooks with that player's XUID, peer index and car position ("down"). Both directions are one
//! STUN indication of type [`KIND`] with one [`ATTR`] attribute; the value is fixed-size and
//! big-endian:
//!
//! ```text
//! byte 0     version (1)          byte 4..6   sequence (u16)
//! byte 1     lights (0..=2)       byte 6      flags, bit 0: a position follows (down only)
//! byte 2     roof (0..=1)         byte 7      zero
//! byte 3     peer index (down)    byte 8..16  XUID (u64)
//! byte 16..28  x, y, z (f32, down only; zero without a position)
//! ```
//!
//! Up values are 16 bytes and down values 28, so a datagram of one direction never reads as the
//! other.

use crate::stun::{self, TxId};

pub const KIND: u16 = 0x3E91;
pub const ATTR: u16 = 0xC0F1;
pub const VERSION: u8 = 1;
/// The game decides the headlights.
pub const LIGHTS_GAME: u8 = 0;
pub const LIGHTS_ON: u8 = 1;
pub const LIGHTS_OFF: u8 = 2;

const UP_SIZE: usize = 16;
const DOWN_SIZE: usize = 28;
const ROOF_MAX: u8 = 1;
/// Offsets inside the value.
const AT_INDEX: usize = 3;
const AT_SEQ: usize = 4;
const AT_FLAGS: usize = 6;
const AT_XUID: usize = 8;
const AT_POSITION: usize = 16;
const POSITION_FLAG: u8 = 0x01;

/// One car's state as its hook reports it.
#[derive(Clone, Copy, Default, Debug, Hash, PartialEq, Eq)]
pub struct State {
    pub lights: u8,
    pub roof: u8,
    pub seq: u16,
}

/// Hook → world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Up {
    pub state: State,
    pub xuid: u64,
}

/// World → hook: another player's state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Down {
    pub state: State,
    pub index: u8,
    pub xuid: u64,
    pub position: Option<[f32; 3]>,
}

/// A STUN-framed datagram of the car-state type (either direction, not yet validated).
pub fn is_carstate(data: &[u8]) -> bool {
    data.starts_with(&KIND.to_be_bytes()) && stun::is_stun(data)
}

/// The attribute value when it has exactly `size` bytes.
fn value_of(data: &[u8], size: usize) -> Option<&[u8]> {
    stun::attribute(data, KIND, ATTR).filter(|v| v.len() == size)
}

fn be_u16(v: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([v[at], v[at + 1]])
}

fn be_u64(v: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&v[at..at + 8]);
    u64::from_be_bytes(b)
}

fn be_f32(v: &[u8], at: usize) -> f32 {
    f32::from_be_bytes([v[at], v[at + 1], v[at + 2], v[at + 3]])
}

/// The common head of both directions, checked: version 1, lights and roof in range.
fn read_head(v: &[u8]) -> Option<(State, u64)> {
    let &[version, lights, roof, ..] = v else {
        return None;
    };
    (version == VERSION && lights <= LIGHTS_OFF && roof <= ROOF_MAX).then(|| {
        let seq = be_u16(v, AT_SEQ);
        (State { lights, roof, seq }, be_u64(v, AT_XUID))
    })
}

pub fn parse_up(data: &[u8]) -> Option<Up> {
    value_of(data, UP_SIZE)
        .and_then(read_head)
        .map(|(state, xuid)| Up { state, xuid })
}

pub fn parse_down(data: &[u8]) -> Option<Down> {
    let v = value_of(data, DOWN_SIZE)?;
    let (state, xuid) = read_head(v)?;
    let has_position = v[AT_FLAGS] & POSITION_FLAG != 0;
    Some(Down {
        state,
        index: v[AT_INDEX],
        xuid,
        position: has_position.then(|| [0, 4, 8].map(|d| be_f32(v, AT_POSITION + d))),
    })
}

/// The value bytes; the caller's state is written as it is (the reader refuses an out-of-range
/// one, so a bad state can only make its own datagram dropped).
fn write_value(size: usize, state: State, xuid: u64) -> Vec<u8> {
    let [seq_hi, seq_lo] = state.seq.to_be_bytes();
    let mut bytes = Vec::with_capacity(size);
    bytes.extend_from_slice(&[VERSION, state.lights, state.roof, 0, seq_hi, seq_lo, 0, 0]);
    bytes.extend_from_slice(&xuid.to_be_bytes());
    bytes.resize(size, 0);
    bytes
}

/// An up datagram with a fresh transaction (what a hook sends; the tests' and tools' builder).
pub fn up(state: State, xuid: u64) -> Vec<u8> {
    let value = write_value(UP_SIZE, state, xuid);
    stun::indication(KIND, &stun::new_txid(), &[(ATTR, &value)])
}

/// A down datagram with a fresh transaction.
pub fn down(d: &Down) -> Vec<u8> {
    down_with_txid(d, &stun::new_txid())
}

/// A down datagram with the caller's transaction (the engine draws it from its seeded stream, so
/// a replay is byte-exact).
pub fn down_with_txid(d: &Down, txid: &TxId) -> Vec<u8> {
    let mut value = write_value(DOWN_SIZE, d.state, d.xuid);
    value[AT_INDEX] = d.index;
    if let Some(xyz) = d.position {
        value[AT_FLAGS] |= POSITION_FLAG;
        for (n, coord) in xyz.iter().enumerate() {
            let at = AT_POSITION + 4 * n;
            value[at..at + 4].copy_from_slice(&coord.to_be_bytes());
        }
    }
    stun::indication(KIND, txid, &[(ATTR, &value)])
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    #[test]
    fn down_bytes_follow_the_documented_layout() {
        let d = Down {
            state: State {
                lights: LIGHTS_ON,
                roof: 1,
                seq: 0x0102,
            },
            index: 5,
            xuid: 0x0009_0000_0000_00ff,
            position: Some([1.0, -2.0, 0.5]),
        };
        let m = down_with_txid(&d, &TXID);
        assert_eq!(m.len(), 20 + 4 + DOWN_SIZE);
        assert_eq!(&m[..4], &[0x3e, 0x91, 0x00, 0x20]);
        assert_eq!(&m[8..20], &TXID);
        assert_eq!(&m[20..24], &[0xc0, 0xf1, 0x00, 0x1c]);
        assert_eq!(
            &m[24..40],
            &[1, 1, 1, 5, 0x01, 0x02, 1, 0, 0, 0x09, 0, 0, 0, 0, 0, 0xff]
        );
        assert_eq!(&m[40..44], &1.0f32.to_be_bytes());
        assert_eq!(&m[44..48], &(-2.0f32).to_be_bytes());
        assert_eq!(&m[48..52], &0.5f32.to_be_bytes());
        assert_eq!(parse_down(&m), Some(d));
    }

    #[test]
    fn up_is_sixteen_bytes_and_carries_no_index_or_flag() {
        let u = up(
            State {
                lights: LIGHTS_GAME,
                roof: 0,
                seq: 65535,
            },
            42,
        );
        assert_eq!(u.len(), 40);
        assert_eq!(&u[24..32], &[1, 0, 0, 0, 0xff, 0xff, 0, 0]);
        assert!(is_carstate(&u));
        assert_eq!(parse_up(&u).map(|x| x.xuid), Some(42));
    }

    #[test]
    fn other_kinds_and_roof_out_of_range_are_refused() {
        let mut u = up(State::default(), 9);
        u[24 + 2] = 2;
        assert_eq!(parse_up(&u), None);
        let mut other = up(State::default(), 9);
        other[1] = 0x93;
        assert!(!is_carstate(&other));
        assert_eq!(parse_up(&other), None);
    }
}
