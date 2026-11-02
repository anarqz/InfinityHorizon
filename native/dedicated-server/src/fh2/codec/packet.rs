// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written from docs/launcher/NERDCORE_WIRE.md (OD-187); no other codec source used.
//! One datagram = one packet (§2): an 88-bit header (see [`HEADER_BITS`]), `count` messages back to back with no
//! length and no alignment, then an optional voice trailer passed through as raw bits.

use super::{
    CodecError,
    bits::{BitReader, BitRun, BitWriter},
    registry::{Body, MAX_CONNECT_ONLY_ID, MAX_MESSAGE_ID, MESSAGE_ID_BITS, Registry},
};

/// Header length in bits: 16 + 16 + 32 + 8 + 16 (§2.1's table).
// SPEC: §2.1/§2.2 call the header "80-bit" and append messages "after bit 80", but the table's
// fields (offset 72, width 16 for `size`) end at bit 88; the table is followed.
pub const HEADER_BITS: usize = 88;
/// Largest packet a receiver accepts, header included (§2.3).
pub const MAX_PACKET_BYTES: u16 = 0x55c;
/// The id of an unsequenced packet (§2.1).
pub const UNSEQUENCED: u16 = 0xffff;

/// The header (§2.1): 16, 16, 32, 8, 16 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub id: u16,
    pub ack: u16,
    pub ack_bits: u32,
    pub count: u8,
    /// Total size in bytes, header included.
    pub size: u16,
}

impl PacketHeader {
    pub fn read(r: &mut BitReader<'_>) -> Result<PacketHeader, CodecError> {
        Ok(PacketHeader {
            id: r.read(16)? as u16,
            ack: r.read(16)? as u16,
            ack_bits: r.read(32)?,
            count: r.read(8)? as u8,
            size: r.read(16)? as u16,
        })
    }

    pub fn write(&self, w: &mut BitWriter) -> Result<(), CodecError> {
        w.write(u32::from(self.id), 16)?;
        w.write(u32::from(self.ack), 16)?;
        w.write(self.ack_bits, 32)?;
        w.write(u32::from(self.count), 8)?;
        w.write(u32::from(self.size), 16)
    }

    /// Reads and checks a datagram's header: `size` at most 1372 bytes (§2.3), at least the
    /// header, and no larger than the datagram.
    pub fn parse(data: &[u8]) -> Result<PacketHeader, CodecError> {
        let h = PacketHeader::read(&mut BitReader::new(data))?;
        if h.size > MAX_PACKET_BYTES {
            return Err(CodecError::Size(h.size));
        }
        // SPEC: unverified (§2.3): a size below the header or past the datagram is rejected;
        // the spec states only the 1372-byte limit.
        if usize::from(h.size) * 8 < HEADER_BITS || usize::from(h.size) > data.len() {
            return Err(CodecError::Size(h.size));
        }
        Ok(h)
    }

    pub fn is_sequenced(&self) -> bool {
        self.id != UNSEQUENCED
    }
}

/// One message: its 7-bit id and decoded body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: u8,
    pub body: Body,
}

impl Message {
    /// Reads one message. `connect_only` accepts only ids 0-6 (§2.4).
    pub fn read(
        reg: &Registry,
        r: &mut BitReader<'_>,
        connect_only: bool,
    ) -> Result<Message, CodecError> {
        let id = r.read(MESSAGE_ID_BITS)? as u8;
        if id > MAX_MESSAGE_ID {
            return Err(CodecError::MessageId(id));
        }
        if connect_only && id > MAX_CONNECT_ONLY_ID {
            return Err(CodecError::Unparsed(id));
        }
        let body = reg.read_body(id, r)?;
        Ok(Message { id, body })
    }

    /// Writes the 7-bit id and the body.
    pub fn write(&self, reg: &Registry, w: &mut BitWriter) -> Result<(), CodecError> {
        if self.id > MAX_MESSAGE_ID {
            return Err(CodecError::MessageId(self.id));
        }
        w.write(u32::from(self.id), MESSAGE_ID_BITS)?;
        reg.write_body(self.id, &self.body, w)
    }
}

/// What follows the decoded messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tail {
    /// Every message parsed: the remaining bits up to `size` (voice, plus the zero padding of the
    /// last byte; empty when the packet ends on a byte boundary after its last message).
    Voice(BitRun),
    /// A message could not be parsed (unknown id, a body the spec does not give, a failed read):
    /// the rest of the packet, from that message's id on, is discarded (§2.4) and kept raw here.
    Unparsed {
        /// The message's id, when its 7 bits were present.
        msg_id: Option<u8>,
        error: CodecError,
        /// The header's `count` (needed to re-encode the packet exactly).
        declared_count: u8,
        rest: BitRun,
    },
}

/// A decoded packet. The header's `count` and `size` are derived when encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub id: u16,
    pub ack: u16,
    pub ack_bits: u32,
    pub messages: Vec<Message>,
    pub tail: Tail,
}

impl Packet {
    /// Decodes a datagram. Header errors fail the whole packet; a message that cannot be parsed
    /// ends the message list ([`Tail::Unparsed`]). Bytes past the header's `size` are ignored.
    // SPEC: unverified (§2.3): trailing bytes past `size` are ignored.
    pub fn decode(data: &[u8], reg: &Registry, connect_only: bool) -> Result<Packet, CodecError> {
        let h = PacketHeader::parse(data)?;
        let mut r = BitReader::with_len(data, usize::from(h.size) * 8);
        r.seek(HEADER_BITS)?;
        let mut messages = Vec::with_capacity(usize::from(h.count));
        for _ in 0..h.count {
            let start = r.pos();
            match Message::read(reg, &mut r, connect_only) {
                Ok(m) => messages.push(m),
                Err(error) => {
                    r.seek(start)?;
                    let msg_id = r.clone().read(MESSAGE_ID_BITS).ok().map(|v| v as u8);
                    let rest = r.read_run(r.remaining())?;
                    return Ok(Packet {
                        id: h.id,
                        ack: h.ack,
                        ack_bits: h.ack_bits,
                        messages,
                        tail: Tail::Unparsed {
                            msg_id,
                            error,
                            declared_count: h.count,
                            rest,
                        },
                    });
                }
            }
        }
        let voice = r.read_run(r.remaining())?;
        Ok(Packet {
            id: h.id,
            ack: h.ack,
            ack_bits: h.ack_bits,
            messages,
            tail: Tail::Voice(voice),
        })
    }

    /// Encodes the packet: header, messages, tail, then `count` and `size` (§2.2). Fails above
    /// 1372 bytes or 255 messages.
    pub fn encode(&self, reg: &Registry) -> Result<Vec<u8>, CodecError> {
        let count = match &self.tail {
            Tail::Voice(_) => self.messages.len(),
            Tail::Unparsed { declared_count, .. } => {
                let c = usize::from(*declared_count);
                if c <= self.messages.len() {
                    return Err(CodecError::Layout);
                }
                c
            }
        };
        let count = u8::try_from(count).map_err(|_| CodecError::TooManyMessages)?;
        let mut w = BitWriter::new();
        PacketHeader {
            id: self.id,
            ack: self.ack,
            ack_bits: self.ack_bits,
            count,
            size: 0,
        }
        .write(&mut w)?;
        for m in &self.messages {
            m.write(reg, &mut w)?;
        }
        match &self.tail {
            Tail::Voice(v) => w.write_run(v)?,
            Tail::Unparsed { rest, .. } => w.write_run(rest)?,
        }
        finish(w)
    }
}

fn finish(mut w: BitWriter) -> Result<Vec<u8>, CodecError> {
    let bytes = w.len().div_ceil(8);
    let size = u16::try_from(bytes).map_err(|_| CodecError::Size(u16::MAX))?;
    if size > MAX_PACKET_BYTES {
        return Err(CodecError::Size(size));
    }
    w.patch(72, u32::from(size), 16)?;
    Ok(w.into_bytes())
}

/// Builds an outgoing packet message by message, keeping it within 1372 bytes (§2.2, §3.5).
#[derive(Debug, Clone)]
pub struct PacketBuilder {
    w: BitWriter,
    count: u8,
    needs_ack: bool,
}

impl PacketBuilder {
    /// Starts a packet with the receive state's `ack` and `ackBits` (§2.2, §3.5).
    pub fn new(ack: u16, ack_bits: u32) -> PacketBuilder {
        let mut w = BitWriter::new();
        // the placeholder header cannot fail: every field fits its width
        let _ = PacketHeader {
            id: UNSEQUENCED,
            ack,
            ack_bits,
            count: 0,
            size: 0,
        }
        .write(&mut w);
        PacketBuilder {
            w,
            count: 0,
            needs_ack: false,
        }
    }

    pub fn count(&self) -> u8 {
        self.count
    }

    /// Whether the packet carries a reliable message and so needs a sequence id (§3.1, §3.5).
    pub fn needs_ack(&self) -> bool {
        self.needs_ack
    }

    /// Bytes used so far.
    pub fn len_bytes(&self) -> usize {
        self.w.len().div_ceil(8)
    }

    /// Appends a message sent with delivery `class`. On any error (including "does not fit",
    /// `Err(Size)`) the packet is left as it was.
    pub fn push(&mut self, reg: &Registry, m: &Message, class: u8) -> Result<(), CodecError> {
        if self.count == u8::MAX {
            return Err(CodecError::TooManyMessages);
        }
        let mark = self.w.len();
        let r = m.write(reg, &mut self.w).and_then(|()| {
            if self.len_bytes() > usize::from(MAX_PACKET_BYTES) {
                Err(CodecError::Size(MAX_PACKET_BYTES))
            } else {
                Ok(())
            }
        });
        if let Err(e) = r {
            self.w.truncate(mark);
            return Err(e);
        }
        self.count += 1;
        if class < super::registry::RELIABLE_CLASS_LIMIT {
            self.needs_ack = true;
        }
        Ok(())
    }

    /// Closes the packet with its id (a sequence number when [`Self::needs_ack`], else
    /// [`UNSEQUENCED`]) and an optional voice trailer.
    pub fn finish(mut self, id: u16, voice: Option<&BitRun>) -> Result<Vec<u8>, CodecError> {
        if let Some(v) = voice {
            self.w.write_run(v)?;
        }
        self.w.patch(0, u32::from(id), 16)?;
        self.w.patch(64, u32::from(self.count), 8)?;
        finish(self.w)
    }
}
