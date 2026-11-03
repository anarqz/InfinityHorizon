// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The car-state side channel (contract §5.3, MATCHMAKING_API §5a): a hook tells the world its
//! car's forced headlights and roof ("up"), and the world passes each player's state to the other
//! hooks with that player's XUID, peer index and car position ("down"). Both directions are one
//! STUN indication of type [`MESSAGE_TYPE`] with one [`ATTRIBUTE_TYPE`] attribute; the value is fixed-size and
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
//! HookReport values are 16 bytes and down values 28, so a datagram of one direction never reads as the
//! other.

use crate::stun::{self, TxId};

pub const MESSAGE_TYPE: u16 = 0x3E91;
pub const ATTRIBUTE_TYPE: u16 = 0xC0F1;
pub const LAYOUT_VERSION: u8 = 1;
/// The game decides the headlights.
pub const LIGHTS_BY_GAME: u8 = 0;
pub const LIGHTS_FORCED_ON: u8 = 1;
pub const LIGHTS_FORCED_OFF: u8 = 2;

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

/// How a car looks to the others: forced headlights, roof, and the hook's counter.
#[derive(Clone, Copy, Default, Debug, Hash, PartialEq, Eq)]
pub struct CarLook {
    pub headlights: u8,
    pub roof_mode: u8,
    pub sequence: u16,
}

/// Hook → world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookReport {
    pub look: CarLook,
    pub xuid: u64,
}

/// World → hook: another player's state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeerLook {
    pub look: CarLook,
    pub peer_index: u8,
    pub xuid: u64,
    pub world_position: Option<[f32; 3]>,
}

/// A STUN-framed datagram of the car-state type (either direction, not yet validated).
pub fn is_car_state(data: &[u8]) -> bool {
    stun::is_framed(data) && data[..2] == MESSAGE_TYPE.to_be_bytes()
}

/// A car-state value of one direction: its exact length tells which.
struct Value<'a>(&'a [u8]);

impl<'a> Value<'a> {
    fn find(data: &'a [u8], len: usize) -> Option<Value<'a>> {
        let bytes = stun::attribute(data, MESSAGE_TYPE, ATTRIBUTE_TYPE)?;
        (bytes.len() == len).then_some(Value(bytes))
    }

    fn array<const N: usize>(&self, at: usize) -> [u8; N] {
        let mut out = [0u8; N];
        out.copy_from_slice(&self.0[at..at + N]);
        out
    }

    /// The fields both directions share, when the version and the ranges hold.
    fn look_and_xuid(&self) -> Option<(CarLook, u64)> {
        let [version, headlights, roof_mode] = self.array::<3>(0);
        if version != LAYOUT_VERSION || headlights > LIGHTS_FORCED_OFF || roof_mode > ROOF_MAX {
            return None;
        }
        let look = CarLook {
            headlights,
            roof_mode,
            sequence: u16::from_be_bytes(self.array(AT_SEQ)),
        };
        Some((look, u64::from_be_bytes(self.array(AT_XUID))))
    }

    fn position(&self) -> Option<[f32; 3]> {
        (self.0[AT_FLAGS] & POSITION_FLAG != 0).then(|| {
            let mut xyz = [0f32; 3];
            for (axis, out) in xyz.iter_mut().enumerate() {
                *out = f32::from_be_bytes(self.array(AT_POSITION + 4 * axis));
            }
            xyz
        })
    }
}

/// A hook's report (16-byte value), checked.
pub fn read_report(data: &[u8]) -> Option<HookReport> {
    let (look, xuid) = Value::find(data, UP_SIZE)?.look_and_xuid()?;
    Some(HookReport { look, xuid })
}

/// The world's copy of another player's look (28-byte value), checked.
pub fn read_peer_look(data: &[u8]) -> Option<PeerLook> {
    let value = Value::find(data, DOWN_SIZE)?;
    let (look, xuid) = value.look_and_xuid()?;
    Some(PeerLook {
        look,
        peer_index: value.0[AT_INDEX],
        xuid,
        world_position: value.position(),
    })
}

/// The shared head written into a zeroed value of `len` bytes; the look goes out as given (the
/// reader refuses an out-of-range one, so a bad look only gets its own datagram dropped).
fn value_bytes(len: usize, look: CarLook, xuid: u64) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    bytes[..3].copy_from_slice(&[LAYOUT_VERSION, look.headlights, look.roof_mode]);
    bytes[AT_SEQ..AT_SEQ + 2].copy_from_slice(&look.sequence.to_be_bytes());
    bytes[AT_XUID..AT_XUID + 8].copy_from_slice(&xuid.to_be_bytes());
    bytes
}

fn framed(txid: &TxId, value: &[u8]) -> Vec<u8> {
    stun::indication(MESSAGE_TYPE, txid, &[(ATTRIBUTE_TYPE, value)])
}

/// A hook's report with a fresh transaction (what a hook sends; the tests' and tools' builder).
pub fn encode_report(look: CarLook, xuid: u64) -> Vec<u8> {
    framed(&stun::random_txid(), &value_bytes(UP_SIZE, look, xuid))
}

/// Another player's look with a fresh transaction.
pub fn encode_peer_look(look: &PeerLook) -> Vec<u8> {
    encode_peer_look_with(look, &stun::random_txid())
}

/// Another player's look with the caller's transaction (the engine draws it from its seeded
/// stream, so a replay is byte-exact).
pub fn encode_peer_look_with(look: &PeerLook, txid: &TxId) -> Vec<u8> {
    let mut value = value_bytes(DOWN_SIZE, look.look, look.xuid);
    value[AT_INDEX] = look.peer_index;
    if let Some(xyz) = look.world_position {
        value[AT_FLAGS] |= POSITION_FLAG;
        let floats = xyz.iter().flat_map(|c| c.to_be_bytes());
        for (slot, byte) in value[AT_POSITION..].iter_mut().zip(floats) {
            *slot = byte;
        }
    }
    framed(txid, &value)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TXID: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

    #[test]
    fn down_bytes_follow_the_documented_layout() {
        let d = PeerLook {
            look: CarLook {
                headlights: LIGHTS_FORCED_ON,
                roof_mode: 1,
                sequence: 0x0102,
            },
            peer_index: 5,
            xuid: 0x0009_0000_0000_00ff,
            world_position: Some([1.0, -2.0, 0.5]),
        };
        let m = encode_peer_look_with(&d, &TXID);
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
        assert_eq!(read_peer_look(&m), Some(d));
    }

    #[test]
    fn up_is_sixteen_bytes_and_carries_no_index_or_flag() {
        let u = encode_report(
            CarLook {
                headlights: LIGHTS_BY_GAME,
                roof_mode: 0,
                sequence: 65535,
            },
            42,
        );
        assert_eq!(u.len(), 40);
        assert_eq!(&u[24..32], &[1, 0, 0, 0, 0xff, 0xff, 0, 0]);
        assert!(is_car_state(&u));
        assert_eq!(read_report(&u).map(|x| x.xuid), Some(42));
    }

    #[test]
    fn other_kinds_and_roof_out_of_range_are_refused() {
        let mut u = encode_report(CarLook::default(), 9);
        u[24 + 2] = 2;
        assert_eq!(read_report(&u), None);
        let mut other = encode_report(CarLook::default(), 9);
        other[1] = 0x93;
        assert!(!is_car_state(&other));
        assert_eq!(read_report(&other), None);
    }
}
