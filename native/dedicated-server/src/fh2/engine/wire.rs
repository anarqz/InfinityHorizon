// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The messages the engine reads and writes, as the engine sees them (NERDCORE_WIRE.md §2, §4, §6),
//! the packet header, the acknowledgement window and the sequence arithmetic.
//!
//! The engine accepts exactly the messages it handles or forwards and stops a packet at any other
//! id: it never guesses a boundary (an unknown layout ends the packet, and the rest of the packet
//! is not applied). The full message catalogue, for decoding any corpus datagram, is the codec's
//! registry (`codec::Registry`); this module is the engine's narrower acceptance set.

use super::bits::{Bits, Rd, Rr, Truncated};

pub const NO_SEQUENCE: u16 = 0xffff;
pub const NO_ACK: u16 = 0xfffe;
pub const HEADER_LEN: usize = 11;
/// The largest object payload (a 14-bit length).
pub const MAX_PAYLOAD_BITS: usize = (1 << 14) - 1;
/// CarPredictionFull's body (message 15).
pub const CAR_BODY_BITS: usize = 710;
pub const SAT_NAV_NODE_MAX: i32 = 0x00ff_ffff;
pub const VERSION: &[u8] = b"6CC152FD";
pub const GAME: &[u8] = b"Flagship";
/// The datagram size limit the game enforces on a received packet (§2.3).
pub const MAX_DATAGRAM: usize = 1372;
/// The largest message that fits a datagram alone: `(1372 - 11) × 8` bits.
pub const MAX_MESSAGE_BITS: usize = (MAX_DATAGRAM - HEADER_LEN) * 8;
/// An object message's prefix before its payload, at most (id, router, key, type, sequence, length).
pub const OBJECT_PREFIX_MAX_BITS: usize = 52;
/// The largest object payload that fits a datagram in one message.
pub const MAX_DATAGRAM_PAYLOAD_BITS: usize = MAX_MESSAGE_BITS - OBJECT_PREFIX_MAX_BITS;
/// The engine flushes a packet before its body passes this many bytes (it leaves room for the
/// header and for a following message boundary well under [`MAX_DATAGRAM`]).
pub const PACKET_BODY_SPLIT: usize = 1189;

/// An object key's kind (§1.7): the two flag bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Own,
    Relay,
    Server,
    Transfer,
}

/// An object key: kind, id, and the owner index of a relay key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Handle {
    pub kind: Kind,
    pub id: u32,
    pub peer: u8,
}

impl Handle {
    pub fn own(id: u32) -> Handle {
        Handle {
            kind: Kind::Own,
            id,
            peer: 0,
        }
    }

    pub fn relay(id: u32, peer: u8) -> Handle {
        Handle {
            kind: Kind::Relay,
            id,
            peer,
        }
    }

    pub fn transfer(id: u32) -> Handle {
        Handle {
            kind: Kind::Transfer,
            id,
            peer: 0,
        }
    }

    pub fn server(id: u32) -> Handle {
        Handle {
            kind: Kind::Server,
            id,
            peer: 0,
        }
    }

    pub fn read(r: &mut Rd<'_>, width: u32) -> Rr<Handle> {
        let a = r.boolean()?;
        let b = r.boolean()?;
        let id = r.read(width)? as u32;
        let kind = match (a, b) {
            (true, true) => Kind::Server,
            (true, false) => Kind::Transfer,
            (false, true) => Kind::Relay,
            (false, false) => Kind::Own,
        };
        let peer = if kind == Kind::Relay {
            r.read(8)? as u8
        } else {
            0
        };
        Ok(Handle { kind, id, peer })
    }

    pub fn write(&self, w: &mut Bits, width: u32) {
        w.boolean(matches!(self.kind, Kind::Server | Kind::Transfer))
            .boolean(matches!(self.kind, Kind::Server | Kind::Relay))
            .push(u64::from(self.id), width);
        if self.kind == Kind::Relay {
            w.push(u64::from(self.peer), 8);
        }
    }
}

/// `(id bits, type bits)` of a router's keys; `None` for a router the engine does not know.
pub fn router_widths(router: u8) -> Option<(u32, u32)> {
    match router {
        1..=3 => Some((7, 0)),
        6 => Some((2, 2)),
        9 | 12 => Some((4, 2)),
        // weather (10) and car meet (11): one server-owned object each
        10 | 11 => Some((1, 0)),
        _ => None,
    }
}

pub fn car_event_bits(id: u8) -> Option<usize> {
    match id {
        20 => Some(352),
        21 => Some(192),
        35 => Some(0),
        _ => None,
    }
}

pub fn traffic_bits(id: u8) -> Option<usize> {
    match id {
        16 => Some(314),
        17 => Some(200),
        71 | 72 => Some(72),
        73 => Some(330),
        74 => Some(185),
        _ => None,
    }
}

/// An object create (7) or update (8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Obj {
    pub create: bool,
    pub router: u8,
    pub handle: Handle,
    pub ty: u8,
    pub seq: u8,
    pub payload: Bits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Open {
        version: Vec<u8>,
        game: Vec<u8>,
        flags: u8,
        user: u64,
        a: u32,
        b: u32,
        attempt: u32,
    },
    OpenResponse {
        flags: u8,
        reason: u8,
        attempt: u32,
        own_attempt: u32,
    },
    Close,
    Ping {
        time: u32,
    },
    Pong {
        echo: u32,
        time: u32,
    },
    TimeSync {
        index: u16,
        time: u32,
    },
    SyncDone {
        attempt: u32,
    },
    Object(Obj),
    Delete {
        router: u8,
        handle: Handle,
        seq: u8,
    },
    Car {
        handle: Handle,
        body: Bits,
    },
    CarEvent {
        kind: u8,
        handle: Handle,
        body: Bits,
    },
    TagCollision {
        a: Handle,
        b: Handle,
        values: [u8; 4],
    },
    TagBuckets {
        cars: Vec<Handle>,
        buckets: Vec<u8>,
    },
    Traffic {
        kind: u8,
        body: Bits,
    },
    DetachPart {
        kind: u8,
        data: Vec<u8>,
    },
    Chat {
        xuid: u64,
        data: Vec<u8>,
    },
    Finalise,
    Inactive {
        handle: Handle,
    },
    GroupSatNav {
        handle: Handle,
        node: i32,
    },
    Other {
        kind: u8,
        body: Bits,
    },
}

impl Msg {
    pub fn id(&self) -> u8 {
        match self {
            Msg::Open { .. } => 0,
            Msg::OpenResponse { .. } => 1,
            Msg::Close => 2,
            Msg::Ping { .. } => 3,
            Msg::Pong { .. } => 4,
            Msg::TimeSync { .. } => 5,
            Msg::SyncDone { .. } => 6,
            Msg::Object(o) => {
                if o.create {
                    7
                } else {
                    8
                }
            }
            Msg::Delete { .. } => 9,
            Msg::Car { .. } => 15,
            Msg::CarEvent { kind, .. } | Msg::Traffic { kind, .. } | Msg::Other { kind, .. } => {
                *kind
            }
            Msg::TagCollision { .. } => 36,
            Msg::TagBuckets { .. } => 37,
            Msg::DetachPart { .. } => 24,
            Msg::Chat { .. } => 33,
            Msg::Finalise => 34,
            Msg::Inactive { .. } => 70,
            Msg::GroupSatNav { .. } => 52,
        }
    }
}

/// The fixed layouts of the messages carried as [`Msg::Other`]: positive = bits, `-1`/`-2` =
/// a key with a 7/2-bit id, `-3`/`-4` = a compact byte string of at most 64/8 bytes.
fn other_layout(id: u8) -> Option<&'static [i32]> {
    Some(match id {
        14 => &[-1, 44],
        26 => &[34],
        27 => &[35],
        31 => &[32],
        32 => &[-3, -3],
        38 | 60 | 64 => &[],
        40 => &[-2, 32],
        41 => &[-2, 1],
        43 => &[-1, 33],
        44 => &[-1, -4, -4, -4, 34],
        45 => &[-1, -4, -4, -4, -4, -4, 59],
        46 => &[-2, 64],
        47 => &[-2, 32],
        49 | 62 => &[1],
        51 => &[48],
        54 => &[384],
        56 => &[224],
        58 => &[96],
        59 => &[-1],
        63 => &[64],
        _ => return None,
    })
}

/// The bucket-list partner name's maximum (65).
pub const BUCKET_LIST_NAME_MAX: usize = 36;

fn skip_other(r: &mut Rd<'_>, id: u8) -> Rr<()> {
    match id {
        // MsgReadyRequestResponse (server to client): the lobby's event key, a 2-bit answer
        42 => {
            Handle::read(r, 2)?;
            r.skip(2)
        }
        // MsgCarMeetNotifyPending: a 2-bit pending action
        29 => r.skip(2),
        // ConfigureGameOptionsForRoadTripEvent: the event, then a u32
        50 => r.skip(64),
        // bucket-list co-op, server to client: 61 the start time; 66-68 one s32
        61 | 66 | 67 | 68 => r.skip(32),
        // bucket-list partner: key, 5-bit universe, found, a name
        65 => {
            Handle::read(r, 7)?;
            r.skip(6)?;
            r.byte_string(BUCKET_LIST_NAME_MAX).map(|_| ())
        }
        39 => {
            Handle::read(r, 2)?;
            let count = r.read(4)?;
            for _ in 0..count {
                Handle::read(r, 7)?;
            }
            Ok(())
        }
        10..=13 => {
            let router = r.read(4)? as u8;
            let (id_bits, type_bits) = router_widths(router).ok_or(Truncated)?;
            if id == 10 {
                r.skip(type_bits as usize)?;
            }
            if id == 13 && router != 3 {
                return Err(Truncated);
            }
            Handle::read(r, id_bits)?;
            if id == 11 || id == 12 {
                r.skip(8)?;
            }
            if id == 11 {
                let n = r.read(14)? as usize;
                r.skip(n)?;
            }
            if id == 13 {
                let n = r.read(16)? as usize;
                r.skip(n * 6)?;
            }
            Ok(())
        }
        _ => {
            for field in other_layout(id).ok_or(Truncated)? {
                match field {
                    -1 => {
                        Handle::read(r, 7)?;
                    }
                    -2 => {
                        Handle::read(r, 2)?;
                    }
                    -3 => {
                        r.byte_string(64)?;
                    }
                    -4 => {
                        r.byte_string(8)?;
                    }
                    n => r.skip(*n as usize)?,
                }
            }
            Ok(())
        }
    }
}

/// Reads one message; `Err` for an unknown id, an unknown router or a cut message (the engine
/// never guesses the next boundary).
pub fn read(r: &mut Rd<'_>) -> Rr<Msg> {
    let id = r.read(7)? as u8;
    Ok(match id {
        0 => Msg::Open {
            version: r.byte_string(38)?,
            game: r.byte_string(16)?,
            flags: r.read(2)? as u8,
            user: r.read(64)?,
            a: r.read(32)? as u32,
            b: r.read(32)? as u32,
            attempt: r.read(32)? as u32,
        },
        1 => Msg::OpenResponse {
            flags: r.read(2)? as u8,
            reason: r.read(3)? as u8,
            attempt: r.read(32)? as u32,
            own_attempt: r.read(32)? as u32,
        },
        2 => Msg::Close,
        3 => Msg::Ping {
            time: r.read(32)? as u32,
        },
        4 => Msg::Pong {
            echo: r.read(32)? as u32,
            time: r.read(32)? as u32,
        },
        5 => Msg::TimeSync {
            index: r.read(16)? as u16,
            time: r.read(32)? as u32,
        },
        6 => Msg::SyncDone {
            attempt: r.read(32)? as u32,
        },
        7 | 8 => {
            let router = r.read(4)? as u8;
            let (id_bits, type_bits) = router_widths(router).ok_or(Truncated)?;
            let handle = Handle::read(r, id_bits)?;
            let ty = if id == 7 && type_bits != 0 {
                r.read(type_bits)? as u8
            } else {
                0
            };
            let seq = r.read(8)? as u8;
            let len = r.read(14)? as usize;
            Msg::Object(Obj {
                create: id == 7,
                router,
                handle,
                ty,
                seq,
                payload: r.take(len)?,
            })
        }
        9 => {
            let router = r.read(4)? as u8;
            let (id_bits, _) = router_widths(router).ok_or(Truncated)?;
            let handle = Handle::read(r, id_bits)?;
            Msg::Delete {
                router,
                handle,
                seq: r.read(8)? as u8,
            }
        }
        15 => Msg::Car {
            handle: Handle::read(r, 7)?,
            body: r.take(CAR_BODY_BITS)?,
        },
        20 | 21 | 35 => {
            let handle = Handle::read(r, 7)?;
            let n = car_event_bits(id).unwrap_or(0);
            Msg::CarEvent {
                kind: id,
                handle,
                body: r.take(n)?,
            }
        }
        36 => {
            let a = Handle::read(r, 7)?;
            let b = Handle::read(r, 7)?;
            let mut values = [0u8; 4];
            for v in &mut values {
                *v = r.read(3)? as u8;
            }
            Msg::TagCollision { a, b, values }
        }
        37 => {
            let mut cars = Vec::with_capacity(12);
            for _ in 0..12 {
                cars.push(Handle::read(r, 7)?);
            }
            let mut buckets = Vec::with_capacity(12);
            for _ in 0..12 {
                buckets.push(r.read(8)? as u8);
            }
            Msg::TagBuckets { cars, buckets }
        }
        16 | 17 | 71..=74 => Msg::Traffic {
            kind: id,
            body: r.take(traffic_bits(id).unwrap_or(0))?,
        },
        24 => {
            let kind = r.read(4)? as u8;
            let n = r.read(11)? as usize;
            Msg::DetachPart {
                kind,
                data: r.read_bytes(n)?,
            }
        }
        33 => {
            let xuid = r.read(64)?;
            let n = r.read(7)? as usize;
            Msg::Chat {
                xuid,
                data: r.read_bytes(n)?,
            }
        }
        34 => Msg::Finalise,
        70 => Msg::Inactive {
            handle: Handle::read(r, 2)?,
        },
        52 => Msg::GroupSatNav {
            handle: Handle::read(r, 2)?,
            node: r.read(32)? as u32 as i32,
        },
        _ => {
            let mut probe = *r;
            skip_other(&mut probe, id)?;
            let n = probe.pos() - r.pos();
            Msg::Other {
                kind: id,
                body: r.take(n)?,
            }
        }
    })
}

/// Writes one message. The engine only writes messages it built or read, so every layout fits.
pub fn write(m: &Msg) -> Bits {
    let mut w = Bits::new();
    w.push(u64::from(m.id()), 7);
    match m {
        Msg::Open {
            version,
            game,
            flags,
            user,
            a,
            b,
            attempt,
        } => {
            w.byte_string(version)
                .byte_string(game)
                .push(u64::from(*flags), 2)
                .push(*user, 64)
                .push(u64::from(*a), 32)
                .push(u64::from(*b), 32)
                .push(u64::from(*attempt), 32);
        }
        Msg::OpenResponse {
            flags,
            reason,
            attempt,
            own_attempt,
        } => {
            w.push(u64::from(*flags), 2)
                .push(u64::from(*reason), 3)
                .push(u64::from(*attempt), 32)
                .push(u64::from(*own_attempt), 32);
        }
        Msg::Close | Msg::Finalise => {}
        Msg::Ping { time } => {
            w.push(u64::from(*time), 32);
        }
        Msg::Pong { echo, time } => {
            w.push(u64::from(*echo), 32).push(u64::from(*time), 32);
        }
        Msg::TimeSync { index, time } => {
            w.push(u64::from(*index), 16).push(u64::from(*time), 32);
        }
        Msg::SyncDone { attempt } => {
            w.push(u64::from(*attempt), 32);
        }
        Msg::Object(o) => {
            let (id_bits, type_bits) = router_widths(o.router).unwrap_or((7, 0));
            w.push(u64::from(o.router), 4);
            o.handle.write(&mut w, id_bits);
            if o.create && type_bits != 0 {
                w.push(u64::from(o.ty), type_bits);
            }
            w.push(u64::from(o.seq), 8)
                .push(o.payload.len() as u64, 14)
                .append(&o.payload);
        }
        Msg::Delete {
            router,
            handle,
            seq,
        } => {
            w.push(u64::from(*router), 4);
            handle.write(&mut w, router_widths(*router).map_or(7, |x| x.0));
            w.push(u64::from(*seq), 8);
        }
        Msg::Car { handle, body } | Msg::CarEvent { handle, body, .. } => {
            handle.write(&mut w, 7);
            w.append(body);
        }
        Msg::TagCollision { a, b, values } => {
            a.write(&mut w, 7);
            b.write(&mut w, 7);
            for v in values {
                w.push(u64::from(*v), 3);
            }
        }
        Msg::TagBuckets { cars, buckets } => {
            for c in cars {
                c.write(&mut w, 7);
            }
            for b in buckets {
                w.push(u64::from(*b), 8);
            }
        }
        Msg::Traffic { body, .. } | Msg::Other { body, .. } => {
            w.append(body);
        }
        Msg::DetachPart { kind, data } => {
            w.push(u64::from(*kind), 4).push(data.len() as u64, 11);
            for b in data {
                w.push(u64::from(*b), 8);
            }
        }
        Msg::Chat { xuid, data } => {
            w.push(*xuid, 64).push(data.len() as u64, 7);
            for b in data {
                w.push(u64::from(*b), 8);
            }
        }
        Msg::Inactive { handle } => handle.write(&mut w, 2),
        Msg::GroupSatNav { handle, node } => {
            handle.write(&mut w, 2);
            w.push(u64::from(*node as u32), 32);
        }
    }
    w
}

/// A message's bits read back (for checks on queued messages); `None` when it does not read.
pub fn read_bits(b: &Bits) -> Option<Msg> {
    read(&mut b.reader()).ok()
}

/// The 88-bit packet header (§2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub id: u16,
    pub ack: u16,
    pub ack_bits: u32,
    pub count: u8,
    pub size: u16,
}

impl Header {
    pub fn parse(d: &[u8]) -> Option<Header> {
        if d.len() < HEADER_LEN {
            return None;
        }
        Some(Header {
            id: u16::from_le_bytes([d[0], d[1]]),
            ack: u16::from_le_bytes([d[2], d[3]]),
            ack_bits: u32::from_le_bytes([d[4], d[5], d[6], d[7]]),
            count: d[8],
            size: u16::from_le_bytes([d[9], d[10]]),
        })
    }
}

/// A packet of whole messages: the header, then the messages' bits back to back, padded to a byte.
pub fn packet<'a, I: IntoIterator<Item = &'a Bits>>(
    messages: I,
    id: u16,
    ack: u16,
    ack_bits: u32,
) -> Vec<u8> {
    let mut body = Bits::new();
    let mut count = 0usize;
    for m in messages {
        body.append(m);
        count += 1;
    }
    let len = HEADER_LEN + body.bytes().len();
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&ack.to_le_bytes());
    out.extend_from_slice(&ack_bits.to_le_bytes());
    out.push(count.min(255) as u8);
    out.extend_from_slice(&(len.min(0xffff) as u16).to_le_bytes());
    out.extend_from_slice(body.bytes());
    out
}

/// The receive window of sequenced packet ids, as the game keeps it (NERDCORE_WIRE.md §2.3,
/// §3.2; MACBOOK's ruling for step 3): ids run modulo 0xffff, an id equal to the newest or in
/// the 32-bit window already is a duplicate (acknowledged, not applied again), and an id 0x3fff
/// or more from the newest (the short way round) is dropped. The C# engine at f81d09c wraps at
/// 2^16 instead; the vectors that cross the wrap are listed in the README.
#[derive(Debug, Clone, Default)]
pub struct AckWindow(crate::fh2::codec::reliable::ReceiveState);

/// What the receive window says of a packet id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// New (or unsequenced): apply its messages.
    Fresh,
    /// Seen already or older than the window: acknowledge, do not apply.
    Duplicate,
    /// 0x3fff or more away from the newest: dropped silently, not acknowledged.
    Dropped,
}

impl AckWindow {
    pub fn header(&self) -> (u16, u32) {
        self.0.ack()
    }

    pub fn accept(&mut self, packet: u16) -> Verdict {
        use crate::fh2::codec::reliable::RecvVerdict;
        match self.0.accept(packet) {
            RecvVerdict::Unsequenced | RecvVerdict::Accepted => Verdict::Fresh,
            RecvVerdict::Duplicate => Verdict::Duplicate,
            RecvVerdict::Dropped => Verdict::Dropped,
        }
    }
}

/// Whether `(ack, bits)` acknowledges `packet` (distances modulo 0xffff, §3.2).
pub fn acknowledges(ack: u16, bits: u32, packet: u16) -> bool {
    if ack == packet {
        return ack != NO_ACK || bits != 0;
    }
    if ack == NO_SEQUENCE || packet == NO_SEQUENCE {
        return false;
    }
    let back = crate::fh2::codec::reliable::id_dist(ack, packet);
    (1..=32).contains(&back) && bits & (1u32 << (back - 1)) != 0
}

/// Object sequences run modulo 255 (§4.4).
pub fn seq_next(s: u8) -> u8 {
    seq_add(s, 1)
}

pub fn seq_add(s: u8, n: u8) -> u8 {
    ((u16::from(s) + u16::from(n)) % 255) as u8
}

/// Whether object sequence `a` is newer than `b`.
pub fn seq_newer(a: u8, b: u8) -> bool {
    (b < a && a - b < 128) || (a < b && b - a > 128)
}

/// Whether packet id `a` is after `b` (half the 16-bit space).
pub fn seq_after(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}
