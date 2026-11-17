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
//!
//! CR-L53 (MATCHMAKING_API §5f, OD-248): the core above stays layout 1 byte for byte; parts and
//! signals ride in [`PARTS_ATTRIBUTE`] (8 bytes, both directions) and mod state in
//! [`MOD_ATTRIBUTE`] (0..=2 per datagram, a ≤ 64-byte blob each, opaque to the world). A reader
//! that knows only the core walks past both.

use crate::stun::{self, TxId};

/// `0xC0FC`: parts and signals, layout 1.
pub const PARTS_ATTRIBUTE: u16 = 0xC0FC;
/// `0xC0FD`: one mod's state blob.
pub const MOD_ATTRIBUTE: u16 = 0xC0FD;
/// The sync id whose holders exchange `0xC0FC` (the registry's `fh2-anna`; a protocol constant).
pub const SYNC_PARTS: u8 = 2;
/// A mod blob's largest length.
pub const MOD_BLOB_MAX: usize = 64;
/// `0xC0FD` attributes one datagram may carry.
pub const MODS_PER_DATAGRAM: usize = 2;
const PARTS_SIZE: usize = 8;
const MOD_HEADER: usize = 4;
const SIGNALS_MAX: u8 = 3;
/// Parts bits 12..15 are zero.
const PARTS_UNUSED: u16 = 0xF000;
/// Parts bit 0 is the roof (equal to the core's byte 2).
const PARTS_ROOF: u16 = 0x0001;

/// `0xC0FC`'s fields: the signals state (0 off, 1 hazards, 2 left, 3 right; never the blink
/// phase) and the open parts (bit set = open).
#[derive(Clone, Copy, Default, Debug, Hash, PartialEq, Eq)]
pub struct Parts {
    pub signals: u8,
    pub parts: u16,
}

/// One `0xC0FD` blob: its sync id, the mod's blob version, and 1..=64 opaque bytes.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct ModBlob {
    pub id: u8,
    pub version: u8,
    len: u8,
    bytes: [u8; MOD_BLOB_MAX],
}

impl ModBlob {
    /// A blob of `data` (1..=64 bytes) for sync id `id` (non-zero).
    pub fn new(id: u8, version: u8, data: &[u8]) -> Option<ModBlob> {
        if id == 0 || data.is_empty() || data.len() > MOD_BLOB_MAX {
            return None;
        }
        let mut bytes = [0u8; MOD_BLOB_MAX];
        bytes[..data.len()].copy_from_slice(data);
        Some(ModBlob {
            id,
            version,
            len: data.len() as u8,
            bytes,
        })
    }

    pub fn data(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    fn value(&self) -> Vec<u8> {
        let mut v = vec![self.id, self.version, self.len, 0];
        v.extend_from_slice(self.data());
        v
    }

    fn read(value: &[u8]) -> Option<ModBlob> {
        let [id, version, len, zero] = *value.get(..MOD_HEADER)? else {
            return None;
        };
        if zero != 0 || value.len() != MOD_HEADER + usize::from(len) {
            return None;
        }
        ModBlob::new(id, version, &value[MOD_HEADER..])
    }
}

/// What a datagram carries past the core: whether the sender speaks §5f at all (any `0xC0FC`
/// or `0xC0FD`, valid or not), its valid parts and its valid blobs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Extensions {
    pub speaks: bool,
    pub parts: Option<Parts>,
    pub mods: [Option<ModBlob>; MODS_PER_DATAGRAM],
}

impl Extensions {
    pub fn mods(&self) -> impl Iterator<Item = &ModBlob> {
        self.mods.iter().flatten()
    }
}

fn read_parts(value: &[u8], roof: u8) -> Option<Parts> {
    if value.len() != PARTS_SIZE || value[0] != LAYOUT_VERSION || value[1] > SIGNALS_MAX {
        return None;
    }
    let parts = u16::from_be_bytes([value[2], value[3]]);
    if parts & PARTS_UNUSED != 0 || (parts & PARTS_ROOF != 0) != (roof != 0) {
        return None;
    }
    Some(Parts {
        signals: value[1],
        parts,
    })
}

/// The §5f attributes of a car-state datagram whose core reads with roof `roof`. Fail closed
/// beyond §5f's refusals: more than one `0xC0FC` drops the parts, more than two `0xC0FD` drop
/// every blob, and a repeated sync id drops every blob of that id.
pub fn read_extensions(data: &[u8], roof: u8) -> Extensions {
    let mut out = Extensions::default();
    let mut parts = Vec::new();
    let mut blobs = Vec::new();
    for (tag, value) in stun::attributes_of(data, MESSAGE_TYPE) {
        match tag {
            PARTS_ATTRIBUTE => parts.push(read_parts(value, roof)),
            MOD_ATTRIBUTE => blobs.push((value.first().copied(), ModBlob::read(value))),
            _ => continue,
        }
        out.speaks = true;
    }
    if let [only] = parts[..] {
        out.parts = only;
    }
    if blobs.len() <= MODS_PER_DATAGRAM {
        let mut slot = 0;
        for (id, blob) in &blobs {
            let repeated = blobs.iter().filter(|(other, _)| other == id).count() > 1;
            if let (Some(b), false) = (blob, repeated) {
                out.mods[slot] = Some(*b);
                slot += 1;
            }
        }
    }
    out
}

/// [`encode_peer_look_with`] plus parts and up to [`MODS_PER_DATAGRAM`] blobs (more are not
/// sent); with neither it is that datagram byte for byte.
pub fn encode_peer_state_with(
    look: &PeerLook,
    parts: Option<Parts>,
    mods: &[ModBlob],
    txid: &TxId,
) -> Vec<u8> {
    let mods = &mods[..mods.len().min(MODS_PER_DATAGRAM)];
    framed_with(txid, &peer_value(look), parts, mods)
}

/// A hook's report with §5f attributes (the tests' and tools' builder; values as given).
pub fn encode_report_with(
    look: CarLook,
    xuid: u64,
    parts: Option<Parts>,
    mods: &[ModBlob],
) -> Vec<u8> {
    let core = value_bytes(UP_SIZE, look, xuid);
    framed_with(&stun::random_txid(), &core, parts, mods)
}

/// The core value, then `0xC0FC` (reserved bytes zero) and the `0xC0FD` blobs, in that order.
fn framed_with(txid: &TxId, core: &[u8], parts: Option<Parts>, mods: &[ModBlob]) -> Vec<u8> {
    let parts_value = parts.map(|p| {
        let mut v = [0u8; PARTS_SIZE];
        v[0] = LAYOUT_VERSION;
        v[1] = p.signals;
        v[2..4].copy_from_slice(&p.parts.to_be_bytes());
        v
    });
    let blob_values: Vec<Vec<u8>> = mods.iter().map(ModBlob::value).collect();
    let mut attrs: Vec<(u16, &[u8])> = vec![(ATTRIBUTE_TYPE, core)];
    if let Some(v) = &parts_value {
        attrs.push((PARTS_ATTRIBUTE, v));
    }
    for v in &blob_values {
        attrs.push((MOD_ATTRIBUTE, v));
    }
    stun::indication(MESSAGE_TYPE, txid, &attrs)
}

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
    framed(txid, &peer_value(look))
}

/// The 28-byte down value of `look`.
fn peer_value(look: &PeerLook) -> Vec<u8> {
    let mut value = value_bytes(DOWN_SIZE, look.look, look.xuid);
    value[AT_INDEX] = look.peer_index;
    if let Some(xyz) = look.world_position {
        value[AT_FLAGS] |= POSITION_FLAG;
        let floats = xyz.iter().flat_map(|c| c.to_be_bytes());
        for (slot, byte) in value[AT_POSITION..].iter_mut().zip(floats) {
            *slot = byte;
        }
    }
    value
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

    /// An up whose core is `look` followed by raw `(type, value)` attributes.
    fn up_with(look: CarLook, extra: &[(u16, &[u8])]) -> Vec<u8> {
        let core = value_bytes(UP_SIZE, look, 42);
        let mut attrs: Vec<(u16, &[u8])> = vec![(ATTRIBUTE_TYPE, &core)];
        attrs.extend_from_slice(extra);
        stun::indication(MESSAGE_TYPE, &TXID, &attrs)
    }

    fn blob_value(id: u8, len: usize) -> Vec<u8> {
        let mut v = vec![id, 3, len as u8, 0];
        v.extend((0..len).map(|i| i as u8));
        v
    }

    #[test]
    fn parts_follow_the_layout_and_its_refusals() {
        let roof_open = CarLook {
            roof_mode: 1,
            ..CarLook::default()
        };
        for signals in 0..=3 {
            for parts in [0x0001u16, 0x0FFF] {
                let p = Parts { signals, parts };
                let up = encode_report_with(roof_open, 42, Some(p), &[]);
                assert_eq!(
                    &up[44..52],
                    &[1, signals, (parts >> 8) as u8, parts as u8, 0, 0, 0, 0]
                );
                let ext = read_extensions(&up, 1);
                assert_eq!((ext.speaks, ext.parts), (true, Some(p)));
                assert_eq!(
                    read_report(&up).map(|r| r.look),
                    Some(roof_open),
                    "the core reads alone"
                );
            }
        }
        let refused = |value: &[u8], roof: u8| {
            let up = up_with(CarLook::default(), &[(PARTS_ATTRIBUTE, value)]);
            let ext = read_extensions(&up, roof);
            assert!(ext.speaks && ext.parts.is_none(), "{value:?}");
        };
        refused(&[1, 4, 0, 0, 0, 0, 0, 0], 0); // signals
        refused(&[1, 0, 0x10, 0, 0, 0, 0, 0], 0); // bit 12
        refused(&[1, 0, 0, 1, 0, 0, 0, 0], 0); // bit 0 open, roof closed
        refused(&[1, 0, 0, 0, 0, 0, 0, 0], 1); // bit 0 closed, roof open
        refused(&[2, 0, 0, 0, 0, 0, 0, 0], 0); // version
        refused(&[1, 0, 0, 0, 0, 0, 0], 0); // length
        // two 0xC0FC: neither is taken
        let p = [1u8, 1, 0, 2, 0, 0, 0, 0];
        let up = up_with(
            CarLook::default(),
            &[(PARTS_ATTRIBUTE, &p), (PARTS_ATTRIBUTE, &p)],
        );
        assert_eq!(read_extensions(&up, 0).parts, None);
        // a layout-1 up speaks no §5f
        assert_eq!(
            read_extensions(&encode_report(CarLook::default(), 1), 0),
            Extensions::default()
        );
    }

    #[test]
    fn blobs_follow_the_layout_and_its_refusals() {
        for len in [1usize, 2, 3, 4, 5, 40, 63, 64] {
            let v = blob_value(1, len);
            let up = up_with(CarLook::default(), &[(MOD_ATTRIBUTE, &v)]);
            assert_eq!(
                up.len(),
                20 + 20 + 4 + (4 + len).next_multiple_of(4),
                "padding"
            );
            let ext = read_extensions(&up, 0);
            let b = ext.mods().next().copied().unwrap();
            assert_eq!((b.id, b.version, b.data()), (1, 3, &v[4..]));
            assert_eq!(read_report(&up).map(|r| r.xuid), Some(42));
        }
        let none = |extra: &[(u16, &[u8])]| {
            let ext = read_extensions(&up_with(CarLook::default(), extra), 0);
            assert!(ext.speaks);
            ext.mods().count()
        };
        let mut l65 = blob_value(1, 64);
        l65[2] = 65;
        l65.push(0);
        assert_eq!(none(&[(MOD_ATTRIBUTE, &l65)]), 0, "L = 65");
        assert_eq!(none(&[(MOD_ATTRIBUTE, &blob_value(1, 0))]), 0, "L = 0");
        let mut zero = blob_value(1, 8);
        zero[3] = 1;
        assert_eq!(none(&[(MOD_ATTRIBUTE, &zero)]), 0, "byte 3");
        let mut short = blob_value(1, 8);
        short.pop();
        assert_eq!(none(&[(MOD_ATTRIBUTE, &short)]), 0, "L past the value");
        assert_eq!(none(&[(MOD_ATTRIBUTE, &blob_value(0, 8))]), 0, "id 0");
        let (one, two) = (blob_value(1, 40), blob_value(5, 8));
        assert_eq!(
            none(&[(MOD_ATTRIBUTE, &one), (MOD_ATTRIBUTE, &two)]),
            2,
            "two blobs"
        );
        assert_eq!(
            none(&[(MOD_ATTRIBUTE, &one), (MOD_ATTRIBUTE, &one)]),
            0,
            "a repeated id"
        );
        let three = blob_value(6, 4);
        assert_eq!(
            none(&[
                (MOD_ATTRIBUTE, &one),
                (MOD_ATTRIBUTE, &two),
                (MOD_ATTRIBUTE, &three)
            ]),
            0,
            "more than two"
        );
        // an unknown attribute between is walked past
        let unknown = [0u8; 6];
        assert_eq!(
            none(&[
                (0xC0FE, &unknown),
                (MOD_ATTRIBUTE, &two),
                (PARTS_ATTRIBUTE, &[0; 8])
            ]),
            1
        );
        assert!(ModBlob::new(1, 1, &[0; 65]).is_none());
        assert!(ModBlob::new(1, 1, &[]).is_none());
    }

    #[test]
    fn a_down_without_extensions_is_layout_1_and_with_them_reads_both_ways() {
        let d = PeerLook {
            look: CarLook {
                headlights: 1,
                roof_mode: 1,
                sequence: 9,
            },
            peer_index: 4,
            xuid: 77,
            world_position: None,
        };
        assert_eq!(
            encode_peer_state_with(&d, None, &[], &TXID),
            encode_peer_look_with(&d, &TXID)
        );
        let p = Parts {
            signals: 3,
            parts: 0x0801,
        };
        let b = ModBlob::new(1, 1, &[5; 40]).unwrap();
        let m = encode_peer_state_with(&d, Some(p), &[b], &TXID);
        assert_eq!(m.len(), 20 + 32 + 12 + 48);
        // an older reader takes the core alone
        assert_eq!(read_peer_look(&m), Some(d));
        let ext = read_extensions(&m, 1);
        assert_eq!(
            (ext.parts, ext.mods().copied().collect::<Vec<_>>()),
            (Some(p), vec![b])
        );
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
