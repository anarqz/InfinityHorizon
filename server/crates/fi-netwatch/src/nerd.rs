//! The NerdCore reader §5d needs (MATCHMAKING_API.md §5d "What the relay decodes"), ported from the
//! world server's C# codec (`Infinity.Core/WorldServer/NerdCodec.cs`, `BitStream.cs`,
//! `NerdMessage.cs`, `NerdPayload.cs`), which stays the oracle.
//!
//! - **Packet:** an 11-byte little-endian header (packet id u16, ack u16, ack bits u32, message
//!   count u8, total size u16), then `count` messages, LSB-first bits. A payload is read only when
//!   the header's size equals the datagram's length (what `NerdCodec.Packet` writes); anything else
//!   (STUN indications, FIUP proofs) is not a NerdCore packet.
//! - **Messages:** a 7-bit id, then its layout. Every layout of `NerdCodec.Read`/`SkipOther` is
//!   walked so a car that is not the first message is still reached; an id the table does not know
//!   ends the packet (never guess the next boundary), as in C#.
//! - **Read:** only CarPredictionFull (id 15: a 7-bit-id handle, then 710 body bits: three raw
//!   `f32` positions, three `s16` velocity axes scaled to ±200, the universe's 5 bits at body bit
//!   395, the game's u32 prediction stamp at body bit 678) and router-1 objects (ids 7 and 8:
//!   PlayerData, only variable 4, the player's XUID, for the owner byte ↔ XUID map; the gamertag
//!   (variable 3) and every other field are skipped, never kept).

/// The header's length.
pub const HEADER_LEN: usize = 11;
/// CarPredictionFull's body, bits (`NerdCodec.CarBodyBits`).
pub const CAR_BODY_BITS: usize = 710;
/// CarPredictionFull's message id.
pub const CAR_MESSAGE: u8 = 15;
/// Body bit of the universe (5 bits).
pub const UNIVERSE_BIT: usize = 395;
/// Body bit of the game's prediction stamp (u32).
pub const STAMP_BIT: usize = 678;
/// Velocity axis scale: an `s16` of 32767 is 200 m/s (V9, inferred).
pub const VELOCITY_SCALE: f64 = 200.0 / 32767.0;
/// The largest position magnitude read as a sample (`NerdPayload.PositionMax`).
pub const POSITION_MAX: f32 = 1e5;

/// LSB-first bit reader over a byte slice (C#'s `BitReader`).
#[derive(Clone, Copy, Debug)]
pub struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Bits<'a> {
    /// A reader over all of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            end: data.len() * 8,
        }
    }

    /// The bit position.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bits left.
    pub fn left(&self) -> usize {
        self.end - self.pos
    }

    /// Reads `width` (0-64) bits, `None` when truncated.
    pub fn read(&mut self, width: usize) -> Option<u64> {
        if width > 64 || width > self.left() {
            return None;
        }
        let mut value = 0u64;
        let mut got = 0;
        while got < width {
            let at = self.pos + got;
            let offset = at & 7;
            let count = (8 - offset).min(width - got);
            let byte = u64::from(self.data[at >> 3] >> offset) & ((1u64 << count) - 1);
            value |= byte << got;
            got += count;
        }
        self.pos += width;
        Some(value)
    }

    /// Skips `width` bits.
    pub fn skip(&mut self, width: usize) -> Option<()> {
        if width > self.left() {
            return None;
        }
        self.pos += width;
        Some(())
    }

    /// A reader over the next `width` bits (and skips them here).
    pub fn take(&mut self, width: usize) -> Option<Bits<'a>> {
        if width > self.left() {
            return None;
        }
        let sub = Bits {
            data: self.data,
            pos: self.pos,
            end: self.pos + width,
        };
        self.pos += width;
        Some(sub)
    }

    fn compact_uint(&mut self) -> Option<u64> {
        let size = self.read(2)? as usize;
        self.read(8 * size + 6)
    }

    fn byte_string(&mut self, maximum: u64) -> Option<()> {
        let count = self.compact_uint()?;
        if count > maximum {
            return None;
        }
        self.skip(count as usize * 8)
    }
}

/// The 11-byte packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Packet id (0xffff: unsequenced).
    pub id: u16,
    /// Messages in the packet.
    pub count: u8,
}

impl Header {
    /// The header of a datagram the world server reads as a NerdCore packet: at least 11 bytes and
    /// not a STUN message (`RelayEngine.Receive`: STUN goes to the side channels first). The size
    /// field is not checked, exactly as `NerdHeader.Parse` (the relay must read what the host reads).
    pub fn parse(data: &[u8]) -> Option<Self> {
        if data.len() < HEADER_LEN || is_stun(data) {
            return None;
        }
        Some(Self {
            id: u16::from_le_bytes([data[0], data[1]]),
            count: data[8],
        })
    }
}

/// `StunCodec.IsStun`: 20 to 548 bytes, the top two bits clear, the magic cookie, the length field
/// covering the rest, a multiple of 4.
pub fn is_stun(data: &[u8]) -> bool {
    (20..=548).contains(&data.len())
        && data[0] & 0xc0 == 0
        && data[4..8] == [0x21, 0x12, 0xa4, 0x42]
        && usize::from(u16::from_be_bytes([data[2], data[3]])) + 20 == data.len()
        && data.len().is_multiple_of(4)
}

/// An object handle's kind (`HandleKind`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleKind {
    /// The sender's own object (joiner → host).
    Own,
    /// Another peer's object, with its owner byte (host → joiner).
    Relay,
    /// A server object.
    Server,
    /// An ownership transfer.
    Transfer,
}

/// An object handle (`ObjectHandle`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle {
    /// The kind.
    pub kind: HandleKind,
    /// The object id.
    pub id: u32,
    /// The owner byte (the world server's peer index), `Relay` only.
    pub owner: u8,
}

fn handle(r: &mut Bits<'_>, width: usize) -> Option<Handle> {
    let a = r.read(1)? != 0;
    let b = r.read(1)? != 0;
    let id = r.read(width)? as u32;
    let kind = match (a, b) {
        (true, true) => HandleKind::Server,
        (true, false) => HandleKind::Transfer,
        (false, true) => HandleKind::Relay,
        (false, false) => HandleKind::Own,
    };
    let owner = if kind == HandleKind::Relay {
        r.read(8)? as u8
    } else {
        0
    };
    Some(Handle { kind, id, owner })
}

/// `(id bits, type bits)` of a router (`NerdCodec.RouterWidths`).
fn router_widths(router: u8) -> Option<(usize, usize)> {
    match router {
        1..=3 => Some((7, 0)),
        6 => Some((2, 2)),
        9 | 12 => Some((4, 2)),
        10 | 11 => Some((1, 0)),
        _ => None,
    }
}

/// One decoded CarPredictionFull.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Car {
    /// Its handle (own on the joiner's leg; with an owner byte on the host's).
    pub handle: Handle,
    /// Position, the body's three raw floats (metres inferred; the second is vertical).
    pub position: [f32; 3],
    /// Velocity, m/s per axis (±200).
    pub velocity: [f32; 3],
    /// Whether an axis is at the scale's end (±32767 or -32768).
    pub velocity_saturated: bool,
    /// The universe (5 bits).
    pub universe: u8,
    /// The game's prediction stamp.
    pub stamp: u32,
}

/// What [`walk`] yields.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// A CarPredictionFull with a finite position within [`POSITION_MAX`].
    Car(Car),
    /// A CarPredictionFull whose position is not finite or out of bounds (counted, never measured).
    BadCar(Handle),
    /// A router-1 object (PlayerData) and the XUID its payload names, when it carries variable 4.
    Player {
        /// Its handle.
        handle: Handle,
        /// Variable 4, when present.
        xuid: Option<u64>,
    },
}

/// How a walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walked {
    /// Not a NerdCore packet: nothing read.
    NotNerd,
    /// Every message was walked.
    Complete,
    /// A message the table cannot place (or a truncated one) ended the read.
    Stopped,
}

fn decode_car(handle: Handle, mut body: Bits<'_>) -> Option<Event> {
    let mut f = [0f32; 3];
    for v in &mut f {
        *v = f32::from_bits(body.read(32)? as u32);
    }
    let mut velocity = [0f32; 3];
    let mut saturated = false;
    for v in &mut velocity {
        let raw = body.read(16)? as u16 as i16;
        saturated |= raw == i16::MAX || raw == i16::MIN || raw == -i16::MAX;
        *v = (f64::from(raw) * VELOCITY_SCALE) as f32;
    }
    body.skip(UNIVERSE_BIT - 144)?;
    let universe = body.read(5)? as u8;
    body.skip(STAMP_BIT - UNIVERSE_BIT - 5)?;
    let stamp = body.read(32)? as u32;
    if f.iter().any(|v| !v.is_finite() || v.abs() > POSITION_MAX) {
        return Some(Event::BadCar(handle));
    }
    Some(Event::Car(Car {
        handle,
        position: f,
        velocity,
        velocity_saturated: saturated,
        universe,
        stamp,
    }))
}

/// Router-1 variable widths 0-11 (`NerdPayload.PlayerVariables`): positive bits, -2 the bounded
/// UTF-16 name (5-bit count, at most 16 units of 16 bits).
const PLAYER_VARIABLES: [i32; 12] = [64, 1, 1, -2, 64, 1, 32, 1, 32, 32, 32, 32];
/// The XUID variable of PlayerData.
pub const PLAYER_XUID_VARIABLE: usize = 4;

/// Variable 4 of a router-1 payload, when present among the mapped twelve; the name and every
/// other value is skipped. `None` on anything malformed, a duplicate or a payload without it.
pub fn player_xuid(mut p: Bits<'_>) -> Option<u64> {
    let mut seen = 0u16;
    while p.left() > 0 {
        let index = p.read(8)? as usize;
        if index >= PLAYER_VARIABLES.len() {
            return None;
        }
        if seen & (1 << index) != 0 {
            return None;
        }
        seen |= 1 << index;
        match PLAYER_VARIABLES[index] {
            -2 => {
                let count = p.read(5)? as usize;
                if count > 16 {
                    return None;
                }
                p.skip(count * 16)?;
            }
            width => {
                let value = p.read(width as usize)?;
                if index == PLAYER_XUID_VARIABLE {
                    return Some(value);
                }
            }
        }
    }
    None
}

/// Fixed layouts of `NerdCodec.OtherLayout`: positive bits, -1/-2 handles of 7/2 id bits, -3/-4
/// compact byte strings of at most 64/8 bytes.
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

fn skip_other(r: &mut Bits<'_>, id: u8) -> Option<()> {
    match id {
        42 => {
            handle(r, 2)?;
            r.skip(2)
        }
        29 => r.skip(2),
        50 => r.skip(64),
        39 => {
            handle(r, 2)?;
            let count = r.read(4)?;
            for _ in 0..count {
                handle(r, 7)?;
            }
            Some(())
        }
        10..=13 => {
            let router = r.read(4)? as u8;
            let (id_bits, type_bits) = router_widths(router)?;
            if id == 10 {
                r.skip(type_bits)?;
            }
            if id == 13 && router != 3 {
                return None;
            }
            handle(r, id_bits)?;
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
            Some(())
        }
        _ => {
            for field in other_layout(id)? {
                match *field {
                    -1 => {
                        handle(r, 7)?;
                    }
                    -2 => {
                        handle(r, 2)?;
                    }
                    -3 => r.byte_string(64)?,
                    -4 => r.byte_string(8)?,
                    bits => r.skip(bits as usize)?,
                }
            }
            Some(())
        }
    }
}

/// Reads one message; `None` ends the packet.
fn message(r: &mut Bits<'_>, visit: &mut impl FnMut(Event)) -> Option<()> {
    let id = r.read(7)? as u8;
    match id {
        0 => {
            r.byte_string(38)?;
            r.byte_string(16)?;
            r.skip(2 + 64 + 32 * 3)
        }
        1 => r.skip(2 + 3 + 32 + 32),
        2 | 34 => Some(()),
        3 => r.skip(32),
        4 => r.skip(64),
        5 => r.skip(16 + 32),
        6 => r.skip(32),
        7 | 8 => {
            let router = r.read(4)? as u8;
            let (id_bits, type_bits) = router_widths(router)?;
            let h = handle(r, id_bits)?;
            if id == 7 && type_bits != 0 {
                r.skip(type_bits)?;
            }
            r.skip(8)?;
            let length = r.read(14)? as usize;
            let payload = r.take(length)?;
            if router == 1 {
                visit(Event::Player {
                    handle: h,
                    xuid: player_xuid(payload),
                });
            }
            Some(())
        }
        9 => {
            let router = r.read(4)? as u8;
            let (id_bits, _) = router_widths(router)?;
            handle(r, id_bits)?;
            r.skip(8)
        }
        CAR_MESSAGE => {
            let h = handle(r, 7)?;
            let body = r.take(CAR_BODY_BITS)?;
            visit(decode_car(h, body)?);
            Some(())
        }
        20 => {
            handle(r, 7)?;
            r.skip(352)
        }
        21 => {
            handle(r, 7)?;
            r.skip(192)
        }
        35 => handle(r, 7).map(|_| ()),
        36 => {
            handle(r, 7)?;
            handle(r, 7)?;
            r.skip(12)
        }
        37 => {
            for _ in 0..12 {
                handle(r, 7)?;
            }
            r.skip(12 * 8)
        }
        16 => r.skip(314),
        17 => r.skip(200),
        71 | 72 => r.skip(72),
        73 => r.skip(330),
        74 => r.skip(185),
        24 => {
            r.skip(4)?;
            let n = r.read(11)? as usize;
            r.skip(n * 8)
        }
        33 => {
            r.skip(64)?;
            let n = r.read(7)? as usize;
            r.skip(n * 8)
        }
        70 => handle(r, 2).map(|_| ()),
        52 => {
            handle(r, 2)?;
            r.skip(32)
        }
        other => skip_other(r, other),
    }
}

/// Walks a datagram's messages and yields the cars and PlayerData objects in order.
pub fn walk(data: &[u8], mut visit: impl FnMut(Event)) -> Walked {
    let Some(header) = Header::parse(data) else {
        return Walked::NotNerd;
    };
    let mut r = Bits::new(&data[HEADER_LEN..]);
    for _ in 0..header.count {
        if r.left() < 7 {
            // `ReceivePacket` stops at fewer than 7 bits left (byte padding).
            return Walked::Complete;
        }
        if message(&mut r, &mut visit).is_none() {
            return Walked::Stopped;
        }
    }
    Walked::Complete
}

/// Test-side writer of synthetic packets by the C# codec's rules (golden vectors; never a real
/// capture). Public so fi-relay-vm's end-to-end test builds the same datagrams.
pub mod synth {
    use super::*;

    /// LSB-first bit writer (C#'s `BitBuffer.Push`).
    #[derive(Clone, Debug, Default)]
    pub struct Writer {
        bytes: Vec<u8>,
        bits: usize,
    }

    impl Writer {
        /// Appends `width` bits of `value`.
        pub fn push(&mut self, value: u64, width: usize) -> &mut Self {
            for i in 0..width {
                if self.bits.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if i < 64 && (value >> i) & 1 == 1 {
                    self.bytes[self.bits >> 3] |= 1 << (self.bits & 7);
                }
                self.bits += 1;
            }
            self
        }

        /// Bits written.
        pub fn len(&self) -> usize {
            self.bits
        }

        /// Whether nothing was written.
        pub fn is_empty(&self) -> bool {
            self.bits == 0
        }

        /// Appends another writer's bits.
        pub fn append(&mut self, other: &Writer) -> &mut Self {
            let mut r = Bits::new(&other.bytes);
            let mut left = other.bits;
            while left > 0 {
                let n = left.min(64);
                let v = r.read(n).unwrap_or(0);
                self.push(v, n);
                left -= n;
            }
            self
        }

        /// The bytes (the last one padded with zeros).
        pub fn bytes(&self) -> &[u8] {
            &self.bytes
        }
    }

    /// Writes a handle (`ObjectHandle.Write`).
    pub fn handle(w: &mut Writer, h: Handle, width: usize) {
        let (a, b) = match h.kind {
            HandleKind::Own => (0, 0),
            HandleKind::Relay => (0, 1),
            HandleKind::Server => (1, 1),
            HandleKind::Transfer => (1, 0),
        };
        w.push(a, 1).push(b, 1).push(u64::from(h.id), width);
        if h.kind == HandleKind::Relay {
            w.push(u64::from(h.owner), 8);
        }
    }

    /// One velocity axis as the game writes it (m/s → s16, rounded).
    pub fn s16(metres_per_second: f64) -> u16 {
        let units = (metres_per_second / VELOCITY_SCALE).round();
        units.clamp(-32768.0, 32767.0) as i16 as u16
    }

    /// A CarPredictionFull message (id, handle, 710 body bits; unknown body bits zero).
    pub fn car(
        h: Handle,
        position: [f32; 3],
        velocity: [f64; 3],
        universe: u8,
        stamp: u32,
    ) -> Writer {
        let mut w = Writer::default();
        w.push(u64::from(CAR_MESSAGE), 7);
        handle(&mut w, h, 7);
        let start = w.len();
        for p in position {
            w.push(u64::from(p.to_bits()), 32);
        }
        for v in velocity {
            w.push(u64::from(s16(v)), 16);
        }
        let pad = UNIVERSE_BIT - (w.len() - start);
        w.push(0, pad);
        w.push(u64::from(universe), 5);
        let pad = STAMP_BIT - (w.len() - start);
        w.push(0, pad);
        w.push(u64::from(stamp), 32);
        debug_assert_eq!(w.len() - start, CAR_BODY_BITS);
        w
    }

    /// A router-1 object (create or update) whose payload holds variable 4 (the XUID) after the
    /// variables 0-3 (a name of `name_units` UTF-16 units of zeros).
    pub fn player(create: bool, h: Handle, xuid: u64, name_units: usize) -> Writer {
        let mut payload = Writer::default();
        payload.push(0, 8).push(0x1234_5678, 64);
        payload.push(1, 8).push(1, 1);
        payload.push(2, 8).push(0, 1);
        payload.push(3, 8).push(name_units as u64, 5);
        for _ in 0..name_units {
            payload.push(0x41, 16);
        }
        payload.push(4, 8).push(xuid, 64);
        payload.push(6, 8).push(7, 32);
        let mut w = Writer::default();
        w.push(if create { 7 } else { 8 }, 7).push(1, 4);
        handle(&mut w, h, 7);
        w.push(3, 8).push(payload.len() as u64, 14);
        w.append(&payload);
        w
    }

    /// A Ping (id 3).
    pub fn ping(time: u32) -> Writer {
        let mut w = Writer::default();
        w.push(3, 7).push(u64::from(time), 32);
        w
    }

    /// A packet of `messages` (`NerdCodec.Packet`): header with the total size.
    pub fn packet(id: u16, messages: &[Writer]) -> Vec<u8> {
        let mut body = Writer::default();
        for m in messages {
            body.append(m);
        }
        let len = HEADER_LEN + body.bytes().len();
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&0xfffeu16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(messages.len() as u8);
        out.extend_from_slice(&(len as u16).to_le_bytes());
        out.extend_from_slice(body.bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::synth::{self, Writer, car, packet, ping, player};
    use super::*;

    fn own(id: u32) -> Handle {
        Handle {
            kind: HandleKind::Own,
            id,
            owner: 0,
        }
    }

    fn relay(id: u32, owner: u8) -> Handle {
        Handle {
            kind: HandleKind::Relay,
            id,
            owner,
        }
    }

    fn events(data: &[u8]) -> (Walked, Vec<Event>) {
        let mut out = Vec::new();
        let w = walk(data, |e| out.push(e));
        (w, out)
    }

    /// Hand-computed golden: id 15 (7 bits 0001111 LSB-first = 0x0F with bit 7 the handle's `a`
    /// = 0), `b` = 0 and id 3 → second byte 0b0000_1100 >> ... computed bit by bit below.
    #[test]
    fn golden_car_hand_computed() {
        // Bits LSB-first: [1,1,1,1,0,0,0] id 15; a=0; b=0; id 3 = [1,1,0,0,0,0,0]; then x = 1.0f32.
        // Byte 0: bits 0-7 = 1,1,1,1,0,0,0,0 (a) → 0x0F. Byte 1: b=0, id bits 1,1,0,0,0,0,0 → 0b0000_0110
        // = 0x06. The body starts at bit 16: x = 0x3F800000 LE → 00 00 80 3F.
        let msg = car(
            own(3),
            [1.0, 2.0, -3.5],
            [10.0, 0.0, -200.0],
            4,
            0xDEAD_BEEF,
        );
        let bytes = msg.bytes();
        assert_eq!(&bytes[..6], &[0x0F, 0x06, 0x00, 0x00, 0x80, 0x3F]);
        // y = 2.0 = 0x40000000; z = -3.5 = 0xC0600000.
        assert_eq!(&bytes[6..14], &[0, 0, 0, 0x40, 0, 0, 0x60, 0xC0]);
        // vx = 10 m/s → round(10 / (200/32767)) = 1638 = 0x0666; vy 0; vz = -200 → -32767 = 0x8001.
        assert_eq!(&bytes[14..20], &[0x66, 0x06, 0, 0, 0x01, 0x80]);
        // The whole message is 7 + 9 + 710 = 726 bits → 91 bytes; the stamp's last byte holds
        // bits 718-725: stamp bits 24-31 = 0xDE spread over bytes 89-90 (726 = 90*8 + 6).
        assert_eq!(msg.len(), 726);
        let pkt = packet(0xffff, &[msg]);
        assert_eq!(pkt.len(), 11 + 91);
        assert_eq!(&pkt[..11], &[0xff, 0xff, 0xfe, 0xff, 0, 0, 0, 0, 1, 102, 0]);
        let (walked, ev) = events(&pkt);
        assert_eq!(walked, Walked::Complete);
        let [Event::Car(c)] = ev.as_slice() else {
            panic!("{ev:?}")
        };
        assert_eq!(c.handle, own(3));
        assert_eq!(c.position, [1.0, 2.0, -3.5]);
        assert!((c.velocity[0] - 10.0).abs() < 0.01);
        assert!((c.velocity[2] + 200.0).abs() < 0.01);
        assert!(c.velocity_saturated, "an axis at -32767 is the scale's end");
        assert_eq!((c.universe, c.stamp), (4, 0xDEAD_BEEF));
    }

    #[test]
    fn a_car_after_other_messages_and_its_owner_byte_are_read() {
        let pkt = packet(
            7,
            &[
                ping(99),
                player(true, relay(5, 2), 2535_0000_0000_0042, 6),
                car(relay(9, 2), [5.0, 0.0, 5.0], [1.0, 0.0, 0.0], 1, 1000),
            ],
        );
        let (walked, ev) = events(&pkt);
        assert_eq!(walked, Walked::Complete);
        assert_eq!(
            ev[0],
            Event::Player {
                handle: relay(5, 2),
                xuid: Some(2535_0000_0000_0042)
            }
        );
        let Event::Car(c) = ev[1] else { panic!() };
        assert_eq!(
            (c.handle.kind, c.handle.owner, c.stamp),
            (HandleKind::Relay, 2, 1000)
        );
        assert!(!c.velocity_saturated);
    }

    #[test]
    fn non_nerd_payloads_and_unknown_messages_stop_without_guessing() {
        // A STUN indication (car state 0x3E91, 4 bytes of attribute) is not a NerdCore packet.
        let mut stun = vec![0x3e, 0x91, 0x00, 0x08, 0x21, 0x12, 0xa4, 0x42];
        stun.extend_from_slice(&[0u8; 12]);
        stun.extend_from_slice(&[0xc0, 0xf1, 0x00, 0x04, 1, 0, 0, 0]);
        assert!(is_stun(&stun));
        assert_eq!(events(&stun).0, Walked::NotNerd);
        assert_eq!(
            events(&[1, 2, 3]).0,
            Walked::NotNerd,
            "shorter than a header"
        );
        // A wrong size field is read anyway (the world server never checks it).
        let mut pkt = packet(1, &[car(own(1), [1.0; 3], [0.0; 3], 0, 5)]);
        pkt[9] ^= 1;
        assert!(matches!(events(&pkt).1.as_slice(), [Event::Car(_)]));
        // An unknown id (127) before a car: the car is never reached.
        let mut unknown = Writer::default();
        unknown.push(127, 7);
        let pkt = packet(1, &[unknown, car(own(1), [0.0; 3], [0.0; 3], 0, 1)]);
        let (walked, ev) = events(&pkt);
        assert_eq!((walked, ev.len()), (Walked::Stopped, 0));
        // A truncated car.
        let mut pkt = packet(1, &[car(own(1), [0.0; 3], [0.0; 3], 0, 1)]);
        pkt.truncate(40);
        let n = pkt.len() as u16;
        pkt[9..11].copy_from_slice(&n.to_le_bytes());
        assert_eq!(events(&pkt).0, Walked::Stopped);
        // A non-finite position is a bad car, never a sample.
        let pkt = packet(1, &[car(own(1), [f32::NAN, 0.0, 0.0], [0.0; 3], 0, 1)]);
        assert_eq!(events(&pkt).1, vec![Event::BadCar(own(1))]);
        let pkt = packet(1, &[car(own(1), [2e5, 0.0, 0.0], [0.0; 3], 0, 1)]);
        assert_eq!(events(&pkt).1, vec![Event::BadCar(own(1))]);
    }

    #[test]
    fn player_data_yields_only_the_xuid() {
        let mut w = Writer::default();
        w.push(3, 8).push(16, 5);
        for _ in 0..16 {
            w.push(0x42, 16);
        }
        w.push(4, 8).push(77, 64);
        assert_eq!(
            player_xuid(Bits::new(w.bytes()).take(w.len()).unwrap()),
            Some(77)
        );
        // A name longer than 16 units, a duplicate or an unmapped index: no XUID.
        let mut w = Writer::default();
        w.push(3, 8).push(17, 5);
        assert_eq!(
            player_xuid(Bits::new(w.bytes()).take(w.len()).unwrap()),
            None
        );
        let mut w = Writer::default();
        w.push(1, 8)
            .push(1, 1)
            .push(1, 8)
            .push(1, 1)
            .push(4, 8)
            .push(5, 64);
        assert_eq!(
            player_xuid(Bits::new(w.bytes()).take(w.len()).unwrap()),
            None
        );
        let mut w = Writer::default();
        w.push(12, 8).push(1, 1);
        assert_eq!(
            player_xuid(Bits::new(w.bytes()).take(w.len()).unwrap()),
            None
        );
        // An update without variable 4.
        let mut w = Writer::default();
        w.push(6, 8).push(1, 32);
        assert_eq!(
            player_xuid(Bits::new(w.bytes()).take(w.len()).unwrap()),
            None
        );
    }

    #[test]
    fn every_layout_of_the_table_is_walked() {
        // Each skipped layout followed by a car: the car is reached.
        let mut cases: Vec<Writer> = Vec::new();
        let mut w = Writer::default();
        w.push(5, 7).push(1, 16).push(2, 32);
        cases.push(w);
        let mut w = Writer::default(); // Delete, router 2
        w.push(9, 7).push(2, 4);
        synth::handle(&mut w, relay(1, 3), 7);
        w.push(0, 8);
        cases.push(w);
        let mut w = Writer::default(); // Object update router 6 (2 id bits, type only on create)
        w.push(8, 7).push(6, 4);
        synth::handle(&mut w, own(1), 2);
        w.push(1, 8).push(10, 14).push(0, 10);
        cases.push(w);
        let mut w = Writer::default(); // Chat
        w.push(33, 7).push(1, 64).push(2, 7).push(0x4142, 16);
        cases.push(w);
        let mut w = Writer::default(); // Other 45: handle, five compact strings, 59 bits
        w.push(45, 7);
        synth::handle(&mut w, own(2), 7);
        for _ in 0..5 {
            w.push(0, 2).push(1, 6).push(0x41, 8);
        }
        w.push(0, 59);
        cases.push(w);
        let mut w = Writer::default(); // 39 starting grid with two cars
        w.push(39, 7);
        synth::handle(&mut w, own(0), 2);
        w.push(2, 4);
        synth::handle(&mut w, relay(1, 1), 7);
        synth::handle(&mut w, relay(2, 2), 7);
        cases.push(w);
        let mut w = Writer::default(); // 13 router 3: 16-bit count × 6
        w.push(13, 7).push(3, 4);
        synth::handle(&mut w, own(1), 7);
        w.push(2, 16).push(0, 12);
        cases.push(w);
        let mut w = Writer::default(); // traffic 73
        w.push(73, 7).push(0, 330);
        cases.push(w);
        for m in cases {
            let pkt = packet(1, &[m, car(own(1), [1.0; 3], [0.0; 3], 0, 5)]);
            let (walked, ev) = events(&pkt);
            assert_eq!(walked, Walked::Complete);
            assert!(matches!(ev.as_slice(), [Event::Car(_)]), "{ev:?}");
        }
        // 13 on another router is unknown in C#: the read stops.
        let mut w = Writer::default();
        w.push(13, 7).push(1, 4);
        synth::handle(&mut w, own(1), 7);
        let pkt = packet(1, &[w, car(own(1), [1.0; 3], [0.0; 3], 0, 5)]);
        assert_eq!(events(&pkt).0, Walked::Stopped);
    }
}
