// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The engine's datagram layer (NERDCORE_WIRE.md §2, §3) on top of the codec: the size limits it
//! enforces, the assembly of a datagram from encoded messages, and the receive window and
//! acknowledgement test over the codec's id arithmetic.

use super::bits::Bits;
use crate::fh2::codec::{
    packet::{MAX_PACKET_BYTES, PacketHeader},
    reliable::{ReceiveState, RecvVerdict, id_dist},
};

pub use crate::fh2::codec::{
    packet::{HEADER_BYTES, UNSEQUENCED},
    reliable::NO_ACK,
};

/// The datagram size limit the game enforces on a received packet (§2.3), in bytes.
pub const MAX_DATAGRAM: usize = MAX_PACKET_BYTES as usize;
/// The largest message that fits a datagram alone: `(1372 - 11) × 8` bits.
pub const MAX_MESSAGE_BITS: usize = (MAX_DATAGRAM - HEADER_BYTES) * 8;
/// An object message's prefix before its payload, at most (id, router, key, type, sequence, length).
pub const OBJECT_PREFIX_MAX_BITS: usize = 52;
/// The largest object payload that fits a datagram in one message.
pub const MAX_DATAGRAM_PAYLOAD_BITS: usize = MAX_MESSAGE_BITS - OBJECT_PREFIX_MAX_BITS;
/// The engine flushes a packet before its body passes this many bytes (it leaves room for the
/// header and for a following message boundary well under [`MAX_DATAGRAM`]).
pub const PACKET_BODY_SPLIT: usize = 1189;

/// The header of a received datagram, read with no size check of its own (the engine applies
/// [`MAX_DATAGRAM`] itself); `None` below the header's length.
pub fn header_of(datagram: &[u8]) -> Option<PacketHeader> {
    PacketHeader::peek(datagram)
}

/// The message bits of a received datagram (everything after the header).
pub fn messages_of(datagram: &[u8]) -> &[u8] {
    datagram.get(HEADER_BYTES..).unwrap_or(&[])
}

/// Builds one datagram from encoded messages: the codec header (with the message count and the
/// total size, each clamped to its field), then the messages back to back, zero-padded to a byte.
pub fn assemble<'a, I: IntoIterator<Item = &'a Bits>>(
    messages: I,
    id: u16,
    ack: u16,
    ack_bits: u32,
) -> Vec<u8> {
    let (joined, count) = messages
        .into_iter()
        .fold((Bits::new(), 0usize), |(mut acc, n), m| {
            acc.append(m);
            (acc, n + 1)
        });
    let total = HEADER_BYTES + joined.bytes().len();
    let header = PacketHeader {
        id,
        ack,
        ack_bits,
        count: u8::try_from(count).unwrap_or(u8::MAX),
        size: u16::try_from(total).unwrap_or(u16::MAX),
    };
    let mut datagram = header.to_bytes();
    datagram.reserve(joined.bytes().len());
    datagram.extend_from_slice(joined.bytes());
    datagram
}

/// The receive window of sequenced packet ids, as the game keeps it (NERDCORE_WIRE.md §2.3,
/// §3.2; MACBOOK's ruling for step 3): ids run modulo 0xffff, an id equal to the newest or in
/// the 32-bit window already is a duplicate (acknowledged, not applied again), and an id 0x3fff
/// or more from the newest (the short way round) is dropped. The C# engine at f81d09c wraps at
/// 2^16 instead; the vectors that cross the wrap are listed in the README.
#[derive(Debug, Clone, Default)]
pub struct AckWindow(ReceiveState);

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
    /// The newest sequenced id accepted, if any.
    pub fn newest(&self) -> Option<u16> {
        self.0.newest()
    }

    /// The `(ack, ackBits)` pair to stamp on the next packet sent back.
    pub fn ack_fields(&self) -> (u16, u32) {
        self.0.ack()
    }

    pub fn accept(&mut self, packet: u16) -> Verdict {
        match self.0.accept(packet) {
            RecvVerdict::Duplicate => Verdict::Duplicate,
            RecvVerdict::Dropped => Verdict::Dropped,
            RecvVerdict::Unsequenced | RecvVerdict::Accepted => Verdict::Fresh,
        }
    }
}

/// Whether a peer's `(ack, ackBits)` acknowledges our packet `packet` (distances modulo 0xffff,
/// §3.2). An unsequenced id never takes part; 0xfffe is a real id here, so `(0xfffe, _)` does
/// acknowledge our packet 0xfffe.
pub fn acknowledges(ack: u16, ack_bits: u32, packet: u16) -> bool {
    if ack == UNSEQUENCED || packet == UNSEQUENCED {
        false
    } else if ack == packet {
        true
    } else {
        match id_dist(ack, packet) {
            back @ 1..=32 => (ack_bits >> (back - 1)) & 1 == 1,
            _ => false,
        }
    }
}
