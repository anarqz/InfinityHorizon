// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! FM5's NerdCore (OD-206, CR-S99, W6): the bit stream, the packet and the message layouts, ported
//! from the LAN lab's `tools/fm5-probe/nerd.py` (FM5's message table at exe 0x14220e370; lobby
//! spec §1-4 and §10.6). FM5 differs from FH2 (`fh2::codec`) and silently breaks if FH2's widths
//! are assumed: a 6-bit message id (FH2 7, 43 messages renumbered), a 3-bit router (FH2 4), the
//! lobby router's 2-bit ids with a 2-bit type, and a 702-bit car message body (FH2 710). So FM5
//! keeps this small codec of its own; nothing is shared with `fh2::`.
//!
//! Verified on FM5's real OpenConnection: the same 11-byte header and strings as FH2, version
//! `D6ED0951`, game `Flagship`. Layouts past the handshake are the lab's; where nerd.py has no
//! verified end (13 NeedsUpdates, the server-to-console messages) the parse stops. The ownership
//! messages (11 Grant, 12 Transfer) are written from the lobby spec §13 (W16), not run yet.

/// The packet header: id u16, ack u16, ack bits u32, message count u8, size u16 (little endian).
pub const HEADER_LEN: usize = 11;
/// FM5 packs the message id in 6 bits (43 messages), FH2 in 7 (77). Read from the real
/// OpenConnection: the version's compact length starts at bit 6, and 6+8+64+8+64+2+64+96 bits are
/// exactly its 39-byte body.
pub const ID_BITS: usize = 6;
pub const NO_SEQ: u16 = 0xFFFF;
pub const NO_ACK: u16 = 0xFFFE;
pub const VERSION: &[u8] = b"D6ED0951";
pub const GAME: &[u8] = b"Flagship";
pub const REASON_OK: u8 = 0;
pub const REASON_RESPONSE_ACK: u8 = 6;

pub const OPEN_CONNECTION: u8 = 0;
pub const OPEN_RESPONSE: u8 = 1;
pub const CLOSE: u8 = 2;
pub const PING: u8 = 3;
pub const PONG: u8 = 4;
pub const TIME_SYNC: u8 = 5;
pub const SYNC_DONE: u8 = 6;

/// FM5's message names by id (nerd.py `NAMES`).
pub const NAMES: [&str; 43] = [
    "OpenConnection",
    "OpenConnectionResponse",
    "CloseConnection",
    "Ping",
    "Pong",
    "TimeSync",
    "ConnectionSyncDone",
    "ObjectCreate",
    "ObjectUpdate",
    "ObjectDelete",
    "ObjectRequestOwnership",
    "ObjectGrantOwnership",
    "ObjectTransferOwnership",
    "ObjectNeedsUpdates",
    "PlayerInput",
    "CarPredictionFull",
    "MsgBreakGlass",
    "MsgCarCollision",
    "MsgCollObjPos",
    "MsgCollObjRest",
    "MsgCollObjDetachPart",
    "MsgCollObjPosVel",
    "MsgChatManagement",
    "MsgTagCollision",
    "MsgTagBuckets",
    "MsgSoccerGoalScored",
    "MsgResetGrid",
    "MsgStartingGrid",
    "MsgStartRace",
    "MsgGameLaunch",
    "MsgGameComplete",
    "MsgIntermissionCountdown",
    "MsgReadyRequest",
    "MsgReadyRequestResponse",
    "MsgInProgressScoreUpdate",
    "MsgLapScore",
    "MsgScoreKeyframe",
    "MsgRequestTeamChange",
    "MsgRequestPlayerBucketChange",
    "MsgVoteToKick",
    "MsgVoteForTrack",
    "MsgAvailableDrivatars",
    "MsgUpdateAI",
];

/// A datagram or a field that does not parse (short, or a string over its limit): the whole
/// packet is dropped, as the lab server's `unreadable packet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// Values packed least significant bit first, with no alignment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Writer {
    bytes: Vec<u8>,
    len: usize,
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    /// Bits written.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn put_bit(&mut self, bit: bool) {
        if self.len.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if bit {
            self.bytes[self.len / 8] |= 1 << (self.len % 8);
        }
        self.len += 1;
    }

    /// The bit at `i` (false past the end).
    pub fn bit(&self, i: usize) -> bool {
        i < self.len && self.bytes[i / 8] >> (i % 8) & 1 == 1
    }

    /// `value`'s low `bits` bits; past 64 the extra bits are zeros (nerd.py masks the same way).
    pub fn put(&mut self, value: u64, bits: usize) -> &mut Writer {
        for i in 0..bits {
            self.put_bit(i < 64 && (value >> i) & 1 == 1);
        }
        self
    }

    /// [`Writer::put`], by value (nerd.py's chained `push`).
    pub fn push(mut self, value: u64, bits: usize) -> Writer {
        self.put(value, bits);
        self
    }

    /// The compact unsigned integer: u2 size, then `8 × size + 6` bits.
    pub fn cuint(mut self, value: u32) -> Writer {
        let size = if value & 0xFFC0_0000 != 0 {
            3
        } else if value & 0x00FF_C000 != 0 {
            2
        } else if value & 0xFFC0 != 0 {
            1
        } else {
            0
        };
        self.put(size, 2);
        self.put(u64::from(value), 8 * size as usize + 6);
        self
    }

    /// A compact length, then the bytes.
    pub fn string(self, text: &[u8]) -> Writer {
        let mut w = self.cuint(text.len() as u32);
        for b in text {
            w.put(u64::from(*b), 8);
        }
        w
    }

    pub fn append(&mut self, other: &Writer) -> &mut Writer {
        for i in 0..other.len {
            self.put_bit(other.bit(i));
        }
        self
    }

    /// The bits, rounded up to whole bytes (trailing bits zero).
    pub fn bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    /// The first 64 bits as a number (tests, small payloads).
    pub fn value(&self) -> u64 {
        (0..self.len.min(64)).fold(0, |v, i| v | (u64::from(self.bit(i)) << i))
    }

    /// `count` bits of `data` from bit `start`; bits past `data` are zeros (nerd.py's shifted
    /// integer).
    pub fn from_bits(data: &[u8], start: usize, count: usize) -> Writer {
        let mut w = Writer::new();
        for i in start..start + count {
            let bit = i / 8 < data.len() && data[i / 8] >> (i % 8) & 1 == 1;
            w.put_bit(bit);
        }
        w
    }
}

pub struct Reader<'a> {
    data: &'a [u8],
    pub position: usize,
    pub length: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader {
            data,
            position: 0,
            length: data.len() * 8,
        }
    }

    /// `count` bits (at most 64) as a number.
    pub fn bits(&mut self, count: usize) -> Result<u64, Malformed> {
        if self.position + count > self.length {
            return Err(Malformed);
        }
        let mut v = 0u64;
        for i in 0..count {
            let at = self.position + i;
            if i < 64 && self.data[at / 8] >> (at % 8) & 1 == 1 {
                v |= 1 << i;
            }
        }
        self.position += count;
        Ok(v)
    }

    pub fn skip(&mut self, count: usize) -> Result<(), Malformed> {
        self.position += count;
        if self.position > self.length {
            return Err(Malformed);
        }
        Ok(())
    }

    pub fn cuint(&mut self) -> Result<u32, Malformed> {
        let size = self.bits(2)? as usize;
        Ok(self.bits(8 * size + 6)? as u32)
    }

    pub fn string(&mut self, limit: usize) -> Result<Vec<u8>, Malformed> {
        let size = self.cuint()? as usize;
        if size > limit {
            return Err(Malformed);
        }
        (0..size).map(|_| self.bits(8).map(|b| b as u8)).collect()
    }
}

/// Whether this ack covers that packet id. As FH2's `acks_packet` (nerd.py).
pub fn acks_packet(ack: u16, ackbits: u32, ident: u16) -> bool {
    if ack == ident {
        return ack != NO_ACK || ackbits != 0;
    }
    let back = ack.wrapping_sub(ident);
    (1..=32).contains(&back) && ackbits & (1 << (back - 1)) != 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub ident: u16,
    pub ack: u16,
    pub ackbits: u32,
    pub count: u8,
    pub size: u16,
}

pub fn header(data: &[u8]) -> Option<Header> {
    if data.len() < HEADER_LEN {
        return None;
    }
    let u16_at = |i: usize| u16::from_le_bytes([data[i], data[i + 1]]);
    Some(Header {
        ident: u16_at(0),
        ack: u16_at(2),
        ackbits: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
        count: data[8],
        size: u16_at(9),
    })
}

/// The messages' bits after the header. `None` when it would not fit the header's fields (more
/// than 255 messages or 65,535 bytes; never the case for what this server sends).
pub fn packet(messages: &[Writer], ident: u16, ack: u16, ackbits: u32) -> Option<Vec<u8>> {
    let mut body = Writer::new();
    for m in messages {
        body.append(m);
    }
    let data = body.bytes();
    let count = u8::try_from(messages.len()).ok()?;
    let size = u16::try_from(HEADER_LEN + data.len()).ok()?;
    let mut out = Vec::with_capacity(HEADER_LEN + data.len());
    out.extend_from_slice(&ident.to_le_bytes());
    out.extend_from_slice(&ack.to_le_bytes());
    out.extend_from_slice(&ackbits.to_le_bytes());
    out.push(count);
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&data);
    Some(out)
}

pub fn open_response(flags: u8, reason: u8, attempt: u32, own_attempt: u32) -> Writer {
    Writer::new()
        .push(u64::from(OPEN_RESPONSE), ID_BITS)
        .push(u64::from(flags), 2)
        .push(u64::from(reason), 3)
        .push(u64::from(attempt), 32)
        .push(u64::from(own_attempt), 32)
}

pub fn ping(time_ms: u32) -> Writer {
    Writer::new()
        .push(u64::from(PING), ID_BITS)
        .push(u64::from(time_ms), 32)
}

pub fn pong(echo: u32, time_ms: u32) -> Writer {
    Writer::new()
        .push(u64::from(PONG), ID_BITS)
        .push(u64::from(echo), 32)
        .push(u64::from(time_ms), 32)
}

pub fn time_sync(index: u16, time_ms: u32) -> Writer {
    Writer::new()
        .push(u64::from(TIME_SYNC), ID_BITS)
        .push(u64::from(index), 16)
        .push(u64::from(time_ms), 32)
}

pub fn sync_done(attempt: u32) -> Writer {
    Writer::new()
        .push(u64::from(SYNC_DONE), ID_BITS)
        .push(u64::from(attempt), 32)
}

/// CloseConnection: the id alone. The lab only read it from consoles; the server sending it
/// (kick, shutdown) is INFERRED from FH2 and untested with FM5.
pub fn close() -> Writer {
    Writer::new().push(u64::from(CLOSE), ID_BITS)
}

// Objects (lobby spec §1-2).
pub const OBJECT_CREATE: u8 = 7;
pub const OBJECT_UPDATE: u8 = 8;
pub const OBJECT_DELETE: u8 = 9;
pub const REQUEST_OWNERSHIP: u8 = 10;
pub const ROUTER_BITS: usize = 3;
pub const ROUTER_PLAYER: u8 = 1;
pub const ROUTER_CAR: u8 = 2;
pub const ROUTER_CONSOLE: u8 = 3;
pub const ROUTER_LOBBY: u8 = 6;
pub const TYPE_SESSION_HOST: u8 = 0;
pub const SEQ_BITS: usize = 8;
pub const SIZE_BITS: usize = 14;
pub const GAME_OPTIONS_INDEX_BITS: usize = 9;
pub const SESSION_HOST_INDEX_BITS: usize = 5;

/// A handle's form: its two leading bits.
pub type Form = (u8, u8);
pub const HANDLE_OWN: Form = (0, 0);
pub const HANDLE_RELAY: Form = (0, 1);
pub const HANDLE_SERVER: Form = (1, 1);
pub const HANDLE_TRANSFER: Form = (1, 0);

/// A router's (id width, type width) in bits; `None`: no known router.
pub fn router_spec(router: u8) -> Option<(usize, usize)> {
    match router {
        ROUTER_PLAYER | ROUTER_CAR | ROUTER_CONSOLE => Some((7, 0)),
        ROUTER_LOBBY => Some((2, 2)),
        _ => None,
    }
}

fn id_bits(router: u8) -> usize {
    router_spec(router).map_or(0, |r| r.0)
}

/// The form, the id (the router's width) and, for a relay handle, the owner's peer byte.
pub fn write_handle(w: &mut Writer, router: u8, form: Form, ident: u32, peer: Option<u8>) {
    w.put(u64::from(form.0), 1)
        .put(u64::from(form.1), 1)
        .put(u64::from(ident), id_bits(router));
    if form == HANDLE_RELAY {
        w.put(u64::from(peer.unwrap_or(0)), 8);
    }
}

pub fn read_handle(r: &mut Reader, bits: usize) -> Result<(Form, u32, Option<u8>), Malformed> {
    let form = (r.bits(1)? as u8, r.bits(1)? as u8);
    let ident = r.bits(bits)? as u32;
    let peer = if form == HANDLE_RELAY {
        Some(r.bits(8)? as u8)
    } else {
        None
    };
    Ok((form, ident, peer))
}

pub fn object_create(
    router: u8,
    form: Form,
    ident: u32,
    kind: u8,
    seq: u8,
    payload: Option<&Writer>,
    peer: Option<u8>,
) -> Writer {
    let mut w = Writer::new()
        .push(u64::from(OBJECT_CREATE), ID_BITS)
        .push(u64::from(router), ROUTER_BITS);
    write_handle(&mut w, router, form, ident, peer);
    let type_bits = router_spec(router).map_or(0, |r| r.1);
    if type_bits > 0 {
        w.put(u64::from(kind), type_bits);
    }
    w.put(u64::from(seq), SEQ_BITS)
        .put(payload.map_or(0, |p| p.len() as u64), SIZE_BITS);
    if let Some(p) = payload {
        w.append(p);
    }
    w
}

pub fn object_update(
    router: u8,
    form: Form,
    ident: u32,
    seq: u8,
    payload: &Writer,
    peer: Option<u8>,
) -> Writer {
    let mut w = Writer::new()
        .push(u64::from(OBJECT_UPDATE), ID_BITS)
        .push(u64::from(router), ROUTER_BITS);
    write_handle(&mut w, router, form, ident, peer);
    w.put(u64::from(seq), SEQ_BITS)
        .put(payload.len() as u64, SIZE_BITS)
        .append(payload);
    w
}

pub fn object_delete(router: u8, form: Form, ident: u32, seq: u8, peer: Option<u8>) -> Writer {
    let mut w = Writer::new()
        .push(u64::from(OBJECT_DELETE), ID_BITS)
        .push(u64::from(router), ROUTER_BITS);
    write_handle(&mut w, router, form, ident, peer);
    w.put(u64::from(seq), SEQ_BITS);
    w
}

// Ownership (lobby spec §1 and §13).
pub const GRANT_OWNERSHIP: u8 = 11;
pub const TRANSFER_OWNERSHIP: u8 = 12;

/// ObjectGrantOwnership (11): router u3, handle, seq u8, size u14, payload (an empty one is legal).
/// The receiver marks the object its own; the seq is the next one of the receiver's link for that
/// object (lobby spec §13).
pub fn grant_ownership(router: u8, form: Form, ident: u32, seq: u8, payload: &Writer) -> Writer {
    let mut w = Writer::new()
        .push(u64::from(GRANT_OWNERSHIP), ID_BITS)
        .push(u64::from(router), ROUTER_BITS);
    write_handle(&mut w, router, form, ident, None);
    w.put(u64::from(seq), SEQ_BITS)
        .put(payload.len() as u64, SIZE_BITS)
        .append(payload);
    w
}

/// ObjectTransferOwnership (12): router u3, handle, seq u8, no size and no payload. The server
/// sends it to take an object's ownership back (lobby spec §13).
pub fn transfer_ownership(router: u8, form: Form, ident: u32, seq: u8) -> Writer {
    let mut w = Writer::new()
        .push(u64::from(TRANSFER_OWNERSHIP), ID_BITS)
        .push(u64::from(router), ROUTER_BITS);
    write_handle(&mut w, router, form, ident, None);
    w.put(u64::from(seq), SEQ_BITS);
    w
}

/// Reads a Grant (11) or a Transfer (12) after its id, as this server writes them (the reader
/// leaves after it). Not in [`read_one`]: consoles are not known to send either, and a packet
/// that has one keeps stopping the parse there as before.
pub fn read_ownership(r: &mut Reader, ident: u8) -> Result<Object, Malformed> {
    let router = r.bits(ROUTER_BITS)? as u8;
    let (id_width, _) = router_spec(router).ok_or(Malformed)?;
    let (form, id, peer) = read_handle(r, id_width)?;
    let seq = r.bits(SEQ_BITS)? as u8;
    let (mut bits, mut payload_at) = (0, r.position);
    if ident == GRANT_OWNERSHIP {
        bits = r.bits(SIZE_BITS)? as usize;
        payload_at = r.position;
        r.skip(bits)?;
    }
    Ok(Object {
        router,
        form,
        id,
        peer,
        kind: None,
        seq,
        bits,
        payload_at,
    })
}

/// An object's payload, as bits, from its packet.
pub fn payload_of(data: &[u8], o: &Object) -> Writer {
    Writer::from_bits(body(data), o.payload_at, o.bits)
}

// Lobby messages (lobby spec §4): the id, the SessionHost's handle, the fields.
pub const RESET_GRID: u8 = 26;
pub const STARTING_GRID: u8 = 27;
pub const START_RACE: u8 = 28;
pub const GAME_LAUNCH: u8 = 29;
pub const GAME_COMPLETE: u8 = 30;
pub const INTERMISSION: u8 = 31;
pub const READY_REQUEST: u8 = 32;
pub const READY_RESPONSE: u8 = 33;
pub const LAUNCH_START: u8 = 0;

pub fn lobby_message(ident: u8) -> Writer {
    let mut w = Writer::new().push(u64::from(ident), ID_BITS);
    write_handle(&mut w, ROUTER_LOBBY, HANDLE_OWN, 0, None);
    w
}

/// MsgReadyRequestResponse: clears the player's ready flag whatever the value (its handler
/// 0x14020bc40 -> 0x140555900 ignores it). In FM5 this message DENIES: never send it as an accept
/// (FH2's ready answer, 42, is the opposite).
pub fn ready_denied(value: u8) -> Writer {
    lobby_message(READY_RESPONSE).push(u64::from(value), 2)
}

/// u32 rising counter, u5 count, each XUID as 8 u8 low byte first (assumed), u2 kind, u32.
pub fn game_launch(counter: u32, xuids: &[u64], kind: u8, extra: u32) -> Writer {
    let mut w = lobby_message(GAME_LAUNCH)
        .push(u64::from(counter), 32)
        .push(xuids.len() as u64, 5);
    for x in xuids {
        w.put(*x, 64);
    }
    w.push(u64::from(kind), 2).push(u64::from(extra), 32)
}

pub fn starting_grid(counter: u32, indexes: &[u8]) -> Writer {
    let mut w = lobby_message(STARTING_GRID)
        .push(u64::from(counter), 32)
        .push(indexes.len() as u64, 5);
    for i in indexes {
        w.put(u64::from(*i), 8);
    }
    w
}

pub fn start_race(time_ms: u32) -> Writer {
    lobby_message(START_RACE).push(u64::from(time_ms), 32)
}

/// Entries of (var index, value, width in bits).
pub fn vars_payload(entries: &[(u32, u64, usize)], index_bits: usize) -> Writer {
    let mut w = Writer::new();
    for (index, value, width) in entries {
        w.put(u64::from(*index), index_bits).put(*value, *width);
    }
    w
}

/// Lobby spec §10.6. Bits after the id; static messages have no handle and are copied as they are.
pub fn static_bits(ident: u8) -> Option<usize> {
    Some(match ident {
        16 => 356,
        17 => 196,
        18 => 400,
        19 => 432,
        21 => 368,
        23 => 16,
        24 => 128,
        25 => 38,
        _ => return None,
    })
}
pub const PLAYER_INPUT: u8 = 14;
pub const CAR_PREDICTION: u8 = 15;
pub const COLL_DETACH: u8 = 20;
pub const CHAT: u8 = 22;
pub const IN_PROGRESS_SCORE: u8 = 34;
pub const LAP_SCORE: u8 = 35;
pub const SCORE_KEYFRAME: u8 = 36;
pub const HANDLED_RELAY: [u8; 5] = [
    PLAYER_INPUT,
    CAR_PREDICTION,
    IN_PROGRESS_SCORE,
    LAP_SCORE,
    SCORE_KEYFRAME,
];
pub const STATIC_RELAY: [u8; 10] = [16, 17, 18, 19, 20, 21, 22, 23, 24, 25];
pub const CAR_ID_BITS: usize = 7;
pub const LOBBY_ID_BITS: usize = 2;

/// An object message's fields (positions counted from the packet body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub router: u8,
    pub form: Form,
    pub id: u32,
    pub peer: Option<u8>,
    /// A create's type, for a router that has one.
    pub kind: Option<u8>,
    pub seq: u8,
    /// The payload's length and start (0 for a delete).
    pub bits: usize,
    pub payload_at: usize,
}

/// A console-handled message's handle (positions counted from the packet body).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle {
    pub form: Form,
    pub id: u32,
    pub peer: Option<u8>,
    pub after_handle: usize,
}

/// One message's fields, as nerd.py's `read_one` gives them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fields {
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
    Ready {
        form: Form,
        id: u32,
        ready: bool,
    },
    Object(Object),
    RequestOwnership {
        router: u8,
        form: Form,
        id: u32,
    },
    /// Read and skipped (the static messages, 20, 22, 43): no fields kept.
    Skipped,
    Handled(Handle),
}

fn read_console_handle(r: &mut Reader, bits: usize) -> Result<Handle, Malformed> {
    let (form, id, peer) = read_handle(r, bits)?;
    Ok(Handle {
        form,
        id,
        peer,
        after_handle: r.position,
    })
}

/// Compact byte string (reader 0x140bb1480): u2 L, count in 6+8L bits, count bytes.
fn read_v(r: &mut Reader) -> Result<(), Malformed> {
    let size = r.bits(2)? as usize;
    let count = r.bits(6 + 8 * size)? as usize;
    r.skip(8 * count)
}

fn read_object(r: &mut Reader, ident: u8) -> Result<Option<Fields>, Malformed> {
    let router = r.bits(ROUTER_BITS)? as u8;
    let Some((id_width, type_width)) = router_spec(router) else {
        return Ok(None);
    };
    let (form, id, peer) = read_handle(r, id_width)?;
    let kind = if ident == OBJECT_CREATE && type_width > 0 {
        Some(r.bits(type_width)? as u8)
    } else {
        None
    };
    let seq = r.bits(SEQ_BITS)? as u8;
    let (mut bits, mut payload_at) = (0, 0);
    if ident != OBJECT_DELETE {
        bits = r.bits(SIZE_BITS)? as usize;
        payload_at = r.position;
        r.skip(bits)?;
    }
    Ok(Some(Fields::Object(Object {
        router,
        form,
        id,
        peer,
        kind,
        seq,
        bits,
        payload_at,
    })))
}

/// The fields of one message, with the reader left after it; `Ok(None)` without a known layout.
pub fn read_one(r: &mut Reader, ident: u8) -> Result<Option<Fields>, Malformed> {
    let f = match ident {
        OPEN_CONNECTION => Fields::Open {
            version: r.string(38)?,
            game: r.string(16)?,
            flags: r.bits(2)? as u8,
            user: r.bits(64)?,
            a: r.bits(32)? as u32,
            b: r.bits(32)? as u32,
            attempt: r.bits(32)? as u32,
        },
        OPEN_RESPONSE => Fields::OpenResponse {
            flags: r.bits(2)? as u8,
            reason: r.bits(3)? as u8,
            attempt: r.bits(32)? as u32,
            own_attempt: r.bits(32)? as u32,
        },
        CLOSE => Fields::Close,
        PING => Fields::Ping {
            time: r.bits(32)? as u32,
        },
        PONG => Fields::Pong {
            echo: r.bits(32)? as u32,
            time: r.bits(32)? as u32,
        },
        TIME_SYNC => Fields::TimeSync {
            index: r.bits(16)? as u16,
            time: r.bits(32)? as u32,
        },
        SYNC_DONE => Fields::SyncDone {
            attempt: r.bits(32)? as u32,
        },
        READY_REQUEST => {
            let (form, id, _) = read_handle(r, LOBBY_ID_BITS)?;
            Fields::Ready {
                form,
                id,
                ready: r.bits(1)? == 1,
            }
        }
        OBJECT_CREATE | OBJECT_UPDATE | OBJECT_DELETE => return read_object(r, ident),
        REQUEST_OWNERSHIP => {
            let router = r.bits(ROUTER_BITS)? as u8;
            let Some((id_width, type_width)) = router_spec(router) else {
                return Ok(None);
            };
            if type_width > 0 {
                r.bits(type_width)?;
            }
            let (form, id, _) = read_handle(r, id_width)?;
            Fields::RequestOwnership { router, form, id }
        }
        COLL_DETACH => {
            r.bits(5)?;
            let n = r.bits(11)? as usize;
            r.skip(8 * n)?;
            Fields::Skipped
        }
        CHAT => {
            r.skip(64)?;
            let n = r.bits(7)? as usize;
            r.skip(8 * n)?;
            Fields::Skipped
        }
        PLAYER_INPUT => {
            let h = read_console_handle(r, CAR_ID_BITS)?;
            r.skip(44)?;
            Fields::Handled(h)
        }
        CAR_PREDICTION => {
            let h = read_console_handle(r, CAR_ID_BITS)?;
            r.skip(702)?;
            Fields::Handled(h)
        }
        IN_PROGRESS_SCORE => {
            let h = read_console_handle(r, CAR_ID_BITS)?;
            r.skip(32)?;
            Fields::Handled(h)
        }
        LAP_SCORE | SCORE_KEYFRAME => {
            let h = read_console_handle(r, CAR_ID_BITS)?;
            for _ in 0..if ident == LAP_SCORE { 3 } else { 5 } {
                read_v(r)?;
            }
            r.skip(if ident == LAP_SCORE {
                32 + 2
            } else {
                32 + 32 + 3
            })?;
            Fields::Handled(h)
        }
        37 | 38 => {
            let h = read_console_handle(r, CAR_ID_BITS)?;
            r.skip(2)?;
            Fields::Handled(h)
        }
        39 | 40 => {
            let h = read_console_handle(r, LOBBY_ID_BITS)?;
            r.skip(if ident == 39 { 64 } else { 2 })?;
            Fields::Handled(h)
        }
        41 => {
            let h = read_console_handle(r, LOBBY_ID_BITS)?;
            let n = r.bits(4)? as usize;
            r.skip(64 * n)?;
            r.bits(3)?;
            Fields::Handled(h)
        }
        43 => {
            r.bits(2)?;
            Fields::Skipped
        }
        id => match static_bits(id) {
            Some(n) => {
                r.skip(n)?;
                Fields::Skipped
            }
            // 13 NeedsUpdates has no verified end, and server-to-console messages are not read
            None => return Ok(None),
        },
    };
    Ok(Some(f))
}

/// One message of a packet: its id, its fields (`None`: no known layout, the last one walked),
/// and its start and end bits in the packet body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walked {
    pub ident: u8,
    pub fields: Option<Fields>,
    pub start: usize,
    pub end: Option<usize>,
}

/// The packet's body: the bits after the header.
pub fn body(data: &[u8]) -> &[u8] {
    data.get(HEADER_LEN..).unwrap_or(&[])
}

/// Each message of the packet, in order. Stops at the first without a known layout: that one
/// comes back with no fields, and the rest of the packet is unread.
pub fn walk_messages(data: &[u8]) -> Result<Vec<Walked>, Malformed> {
    let count = header(data).ok_or(Malformed)?.count;
    let mut out = Vec::new();
    let mut r = Reader::new(body(data));
    for _ in 0..count {
        let start = r.position;
        let ident = r.bits(ID_BITS)? as u8;
        match read_one(&mut r, ident)? {
            None => {
                out.push(Walked {
                    ident,
                    fields: None,
                    start,
                    end: None,
                });
                break;
            }
            Some(fields) => out.push(Walked {
                ident,
                fields: Some(fields),
                start,
                end: Some(r.position),
            }),
        }
    }
    Ok(out)
}

/// (id, fields) of each message (nerd.py `read_messages`).
pub fn read_messages(data: &[u8]) -> Result<Vec<(u8, Option<Fields>)>, Malformed> {
    Ok(walk_messages(data)?
        .into_iter()
        .map(|w| (w.ident, w.fields))
        .collect())
}

/// Those bits of a packet's body.
pub fn bit_slice(data: &[u8], start: usize, end: usize) -> Writer {
    Writer::from_bits(body(data), start, end.saturating_sub(start))
}

/// The id, the handle now naming the owner (relay form, the id, `peer`), and the rest bit for bit.
pub fn relay_with_handle(data: &[u8], ident: u8, h: &Handle, end: usize, peer: u8) -> Writer {
    let mut w = Writer::new()
        .push(u64::from(ident), ID_BITS)
        .push(0, 1)
        .push(1, 1)
        .push(u64::from(h.id), CAR_ID_BITS)
        .push(u64::from(peer), 8);
    w.append(&bit_slice(data, h.after_handle, end));
    w
}

pub fn name(ident: u8) -> &'static str {
    NAMES.get(usize::from(ident)).copied().unwrap_or("unknown")
}
