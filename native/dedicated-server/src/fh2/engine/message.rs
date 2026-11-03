// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The messages the engine understands, as it decodes and encodes them on its own 64-bit bit
//! buffer (NERDCORE_WIRE.md §2.4, §4, §6).
//!
//! The engine accepts exactly the messages it handles or forwards and stops a packet at any other
//! id: it never guesses a boundary (an unknown layout ends the packet, and the rest of the packet
//! is not applied). The full message catalogue, for decoding any corpus datagram, is the codec's
//! registry (`codec::Registry`); the engine takes its router widths, fixed body sizes and the
//! layouts of the bodies it only forwards from that registry, but keeps its own, narrower
//! acceptance set.

use super::{
    bits::{Bits, Rd, Rr, Truncated},
    key::ObjKey,
};
use crate::fh2::codec::{
    fields::{BLOB64_LEN, Field, fixed_bits},
    object::{
        MSG_GRANT_OWNERSHIP, MSG_NEEDS_UPDATES, MSG_OBJECT_CREATE, MSG_OBJECT_DELETE,
        MSG_OBJECT_UPDATE, MSG_REQUEST_OWNERSHIP, MSG_TRANSFER_OWNERSHIP, NEEDS_UPDATES_COUNT_BITS,
        NEEDS_UPDATES_ENTRY_BITS, NEEDS_UPDATES_ROUTER, PAYLOAD_LEN_BITS, ROUTER_ID_BITS, SEQ_BITS,
        router,
    },
    registry::{BodyKind, FH2_MESSAGES, MESSAGE_ID_BITS, MessageSpec},
};

/// The protocol version string a peer opens with.
pub const PROTOCOL_VERSION: &[u8] = b"6CC152FD";
/// The game title string a peer opens with.
pub const GAME_TITLE: &[u8] = b"Flagship";
/// CarPredictionFull's body after its car key (message 15), from the registry row.
pub const CAR_BODY_BITS: usize = registry_fixed_body(15, 1);
/// The largest group sat-nav node a peer may set.
pub const SAT_NAV_NODE_MAX: i32 = 0x00ff_ffff;
/// The bucket-list partner name's maximum (65).
pub const BUCKET_LIST_NAME_MAX: usize = 36;

const ID_BITS: u32 = MESSAGE_ID_BITS as u32;
const ROUTER_BITS: u32 = ROUTER_ID_BITS as u32;
const SEQUENCE_BITS: u32 = SEQ_BITS as u32;
const LENGTH_BITS: u32 = PAYLOAD_LEN_BITS as u32;
/// Key width of a player or car (routers 1-3).
const PLAYER_KEY_BITS: u32 = 7;
/// Key width of a lobby object (router 6).
const LOBBY_KEY_BITS: u32 = 2;
/// The id bits a key is written with when its router is not hosted.
const FALLBACK_KEY_BITS: u32 = PLAYER_KEY_BITS;
/// Byte maxima of OpenConnection's version and title strings.
const VERSION_MAX: usize = 38;
const TITLE_MAX: usize = 16;
/// Cars (and buckets) of a MsgTagBuckets.
const TAG_SLOTS: usize = 12;
/// The client traffic router: registered by the client, but the engine never hosts its objects.
const CLIENT_TRAFFIC_ROUTER: u8 = 8;

/// The body after a message's first `skip` registry fields, for a row of fixed-width fields
/// (checked when the constant tables below are built).
const fn registry_fixed_body(id: u8, skip: usize) -> usize {
    let row = &FH2_MESSAGES[id as usize];
    assert!(row.id == id);
    match row.body {
        BodyKind::Fields(layout) => match fixed_bits(layout.split_at(skip).1) {
            Some(bits) => bits,
            None => panic!("the registry row has a variable-width field"),
        },
        _ => panic!("the registry row is not a field list"),
    }
}

/// The car events the engine relays, with their body after the car key: MsgBreakGlass,
/// MsgCarCollision, MsgRepairDamage.
const CAR_EVENT_BODIES: [(u8, usize); 3] = [
    (20, registry_fixed_body(20, 1)),
    (21, registry_fixed_body(21, 1)),
    (35, registry_fixed_body(35, 1)),
];

/// The server-traffic messages the engine relays, with their whole body: full sync, route
/// progress, the traffic message pair, full route, stationary.
const TRAFFIC_BODIES: [(u8, usize); 6] = [
    (16, registry_fixed_body(16, 0)),
    (17, registry_fixed_body(17, 0)),
    (71, registry_fixed_body(71, 0)),
    (72, registry_fixed_body(72, 0)),
    (73, registry_fixed_body(73, 0)),
    (74, registry_fixed_body(74, 0)),
];

fn body_in(table: &[(u8, usize)], id: u8) -> Option<usize> {
    table
        .iter()
        .find_map(|&(message, bits)| (message == id).then_some(bits))
}

/// The body width after the car key of car event `id`; `None` for any other message.
pub fn car_event_body_bits(id: u8) -> Option<usize> {
    body_in(&CAR_EVENT_BODIES, id)
}

/// The body width of relayed server-traffic message `id`; `None` for any other message.
pub fn traffic_body_bits(id: u8) -> Option<usize> {
    body_in(&TRAFFIC_BODIES, id)
}

/// The messages the engine forwards as [`Message::Opaque`]: their bodies are walked with the
/// registry's layout only to find where they end. Ids outside this set end the packet.
fn forwards_opaque(id: u8) -> bool {
    matches!(
        id,
        10..=14 | 26 | 27 | 29 | 31 | 32 | 38..=47 | 49..=51 | 54 | 56 | 58..=68
    )
}

fn registry_row(id: u8) -> Option<&'static MessageSpec> {
    FH2_MESSAGES.get(usize::from(id)).filter(|row| row.id == id)
}

/// The key and type widths of a router the engine hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyLayout {
    pub id_bits: u32,
    /// 0 when the router's creates carry no type field.
    pub type_bits: u32,
}

impl KeyLayout {
    /// The layout of a hosted router: every router the codec knows but the client traffic one.
    pub fn of(router_id: u8) -> Option<KeyLayout> {
        let spec = router(router_id).filter(|spec| spec.id != CLIENT_TRAFFIC_ROUTER)?;
        Some(KeyLayout {
            id_bits: u32::from(spec.id_bits()),
            type_bits: if spec.has_type_field() {
                u32::from(spec.type_bits())
            } else {
                0
            },
        })
    }

    fn id_bits_or_fallback(router_id: u8) -> u32 {
        KeyLayout::of(router_id).map_or(FALLBACK_KEY_BITS, |layout| layout.id_bits)
    }
}

/// An object create (7) or update (8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectChange {
    pub is_create: bool,
    pub router_id: u8,
    pub key: ObjKey,
    /// The object type (creates only; 0 on an update).
    pub ty: u8,
    pub sequence: u8,
    /// The synced-variable payload.
    pub vars: Bits,
}

impl ObjectChange {
    pub fn message_id(&self) -> u8 {
        if self.is_create {
            MSG_OBJECT_CREATE
        } else {
            MSG_OBJECT_UPDATE
        }
    }

    fn decode(is_create: bool, r: &mut Rd<'_>) -> Rr<ObjectChange> {
        let router_id = r.read(ROUTER_BITS)? as u8;
        let layout = KeyLayout::of(router_id).ok_or(Truncated)?;
        let key = ObjKey::read(r, layout.id_bits)?;
        let ty = if is_create {
            r.read(layout.type_bits)? as u8
        } else {
            0
        };
        let sequence = r.read(SEQUENCE_BITS)? as u8;
        let length = r.read(LENGTH_BITS)? as usize;
        Ok(ObjectChange {
            is_create,
            router_id,
            key,
            ty,
            sequence,
            vars: r.take(length)?,
        })
    }

    fn encode_into(&self, out: &mut Bits) {
        let layout = KeyLayout::of(self.router_id).unwrap_or(KeyLayout {
            id_bits: FALLBACK_KEY_BITS,
            type_bits: 0,
        });
        out.push(u64::from(self.router_id), ROUTER_BITS);
        self.key.write(out, layout.id_bits);
        if self.is_create {
            out.push(u64::from(self.ty), layout.type_bits);
        }
        out.push(u64::from(self.sequence), SEQUENCE_BITS);
        out.push(self.vars.len() as u64, LENGTH_BITS);
        out.append(&self.vars);
    }
}

/// One message the engine handles or forwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// OpenConnection (0).
    Connect {
        version: Vec<u8>,
        game: Vec<u8>,
        flags: u8,
        user: u64,
        a: u32,
        b: u32,
        attempt: u32,
    },
    /// OpenConnectionResponse (1).
    ConnectReply {
        flags: u8,
        reason: u8,
        attempt: u32,
        own_attempt: u32,
    },
    /// CloseConnection (2).
    Disconnect,
    /// Ping (3): the sender's clock.
    Ping { clock: u32 },
    /// Pong (4): two clock fields. The engine answers a Ping with that ping's clock first and its
    /// own clock second; a peer's Pong to the engine's Ping carries a zero first and the engine's
    /// ping clock second (the round-trip time comes from that field).
    Pong { echoed: u32, clock: u32 },
    /// TimeSync (5): the sync round, then the server clock.
    ClockSync { round: u16, clock: u32 },
    /// ConnectionSyncDone (6).
    SyncComplete { attempt: u32 },
    /// ObjectCreate (7) or ObjectUpdate (8).
    Object(ObjectChange),
    /// ObjectDelete (9).
    ObjectDelete {
        router_id: u8,
        key: ObjKey,
        sequence: u8,
    },
    /// CarPredictionFull (15): the car's key, then its fixed body.
    CarState { car: ObjKey, body: Bits },
    /// A car event (20, 21, 35): the car's key, then a fixed body.
    CarEvent { id: u8, car: ObjKey, body: Bits },
    /// MsgTagCollision (36).
    TagCollision {
        first: ObjKey,
        second: ObjKey,
        values: [u8; 4],
    },
    /// MsgTagBuckets (37).
    TagBuckets { cars: Vec<ObjKey>, buckets: Vec<u8> },
    /// A server-traffic message (16, 17, 71-74) with its fixed body.
    TrafficRelay { id: u8, body: Bits },
    /// MsgCollObjDetachPart (24).
    DetachPart { part: u8, bytes: Vec<u8> },
    /// MsgChatManagement (33).
    Chat { xuid: u64, bytes: Vec<u8> },
    /// FinaliseSessionEntry (34).
    Finalise,
    /// MsgInactivePlayersPulled (70): the lobby key.
    Inactive { key: ObjKey },
    /// SetGroupSatNavEvent (52).
    GroupSatNav { key: ObjKey, node: i32 },
    /// A forwarded message: its id and its body bits as received.
    Opaque { id: u8, body: Bits },
}

fn word(r: &mut Rd<'_>) -> Rr<u32> {
    Ok(r.read(32)? as u32)
}

fn narrow(r: &mut Rd<'_>, bits: u32) -> Rr<u8> {
    Ok(r.read(bits)? as u8)
}

fn counted_bytes(r: &mut Rd<'_>, count_bits: u32) -> Rr<Vec<u8>> {
    let count = r.read(count_bits)? as usize;
    r.read_bytes(count)
}

fn put_counted_bytes(out: &mut Bits, bytes: &[u8], count_bits: u32) {
    out.push(bytes.len() as u64, count_bits);
    bytes.iter().for_each(|byte| {
        out.push(u64::from(*byte), 8);
    });
}

impl Message {
    /// The 7-bit message id.
    pub fn wire_id(&self) -> u8 {
        match self {
            Message::Connect { .. } => 0,
            Message::ConnectReply { .. } => 1,
            Message::Disconnect => 2,
            Message::Ping { .. } => 3,
            Message::Pong { .. } => 4,
            Message::ClockSync { .. } => 5,
            Message::SyncComplete { .. } => 6,
            Message::Object(object) => object.message_id(),
            Message::ObjectDelete { .. } => MSG_OBJECT_DELETE,
            Message::CarState { .. } => 15,
            Message::DetachPart { .. } => 24,
            Message::Chat { .. } => 33,
            Message::Finalise => 34,
            Message::TagCollision { .. } => 36,
            Message::TagBuckets { .. } => 37,
            Message::GroupSatNav { .. } => 52,
            Message::Inactive { .. } => 70,
            Message::CarEvent { id, .. }
            | Message::TrafficRelay { id, .. }
            | Message::Opaque { id, .. } => *id,
        }
    }

    /// Decodes one message; `Err` for an id outside the engine's set, an unhosted router or a cut
    /// message (the engine never guesses the next boundary).
    pub fn decode(r: &mut Rd<'_>) -> Rr<Message> {
        let id = r.read(ID_BITS)? as u8;
        match id {
            0..=6 => Message::decode_connection(id, r),
            MSG_OBJECT_CREATE => ObjectChange::decode(true, r).map(Message::Object),
            MSG_OBJECT_UPDATE => ObjectChange::decode(false, r).map(Message::Object),
            MSG_OBJECT_DELETE => Message::decode_delete(r),
            15 | 20 | 21 | 35..=37 => Message::decode_car(id, r),
            24 | 33 | 34 | 52 | 70 => Message::decode_session(id, r),
            _ => match traffic_body_bits(id) {
                Some(bits) => Ok(Message::TrafficRelay {
                    id,
                    body: r.take(bits)?,
                }),
                None => Message::decode_opaque(id, r),
            },
        }
    }

    /// A message decoded back from its encoded bits (for checks on queued messages); `None` when
    /// it does not decode.
    pub fn from_bits(bits: &Bits) -> Option<Message> {
        Message::decode(&mut bits.reader()).ok()
    }

    /// Encodes the message: its id, then its body. The engine only encodes messages it built or
    /// decoded, so every layout fits.
    pub fn encode(&self) -> Bits {
        let mut encoded = Bits::new();
        encoded.push(u64::from(self.wire_id()), ID_BITS);
        self.encode_body(&mut encoded);
        encoded
    }

    fn decode_connection(id: u8, r: &mut Rd<'_>) -> Rr<Message> {
        Ok(match id {
            0 => Message::Connect {
                version: r.byte_string(VERSION_MAX)?,
                game: r.byte_string(TITLE_MAX)?,
                flags: narrow(r, 2)?,
                user: r.read(64)?,
                a: word(r)?,
                b: word(r)?,
                attempt: word(r)?,
            },
            1 => Message::ConnectReply {
                flags: narrow(r, 2)?,
                reason: narrow(r, 3)?,
                attempt: word(r)?,
                own_attempt: word(r)?,
            },
            2 => Message::Disconnect,
            3 => Message::Ping { clock: word(r)? },
            4 => Message::Pong {
                echoed: word(r)?,
                clock: word(r)?,
            },
            5 => Message::ClockSync {
                round: r.read(16)? as u16,
                clock: word(r)?,
            },
            6 => Message::SyncComplete { attempt: word(r)? },
            _ => return Err(Truncated),
        })
    }

    fn decode_delete(r: &mut Rd<'_>) -> Rr<Message> {
        let router_id = r.read(ROUTER_BITS)? as u8;
        let layout = KeyLayout::of(router_id).ok_or(Truncated)?;
        Ok(Message::ObjectDelete {
            router_id,
            key: ObjKey::read(r, layout.id_bits)?,
            sequence: narrow(r, SEQUENCE_BITS)?,
        })
    }

    fn decode_car(id: u8, r: &mut Rd<'_>) -> Rr<Message> {
        match id {
            15 => {
                let car = ObjKey::read(r, PLAYER_KEY_BITS)?;
                let state = r.take(CAR_BODY_BITS)?;
                Ok(Message::CarState { car, body: state })
            }
            36 => {
                let first = ObjKey::read(r, PLAYER_KEY_BITS)?;
                let second = ObjKey::read(r, PLAYER_KEY_BITS)?;
                let mut values = [0u8; 4];
                for value in values.iter_mut() {
                    *value = narrow(r, 3)?;
                }
                Ok(Message::TagCollision {
                    first,
                    second,
                    values,
                })
            }
            37 => {
                let cars = (0..TAG_SLOTS)
                    .map(|_| ObjKey::read(r, PLAYER_KEY_BITS))
                    .collect::<Rr<Vec<_>>>()?;
                let buckets = (0..TAG_SLOTS)
                    .map(|_| narrow(r, 8))
                    .collect::<Rr<Vec<_>>>()?;
                Ok(Message::TagBuckets { cars, buckets })
            }
            _ => {
                let width = car_event_body_bits(id).ok_or(Truncated)?;
                let car = ObjKey::read(r, PLAYER_KEY_BITS)?;
                Ok(Message::CarEvent {
                    id,
                    car,
                    body: r.take(width)?,
                })
            }
        }
    }

    fn decode_session(id: u8, r: &mut Rd<'_>) -> Rr<Message> {
        Ok(match id {
            24 => Message::DetachPart {
                part: narrow(r, 4)?,
                bytes: counted_bytes(r, 11)?,
            },
            33 => Message::Chat {
                xuid: r.read(64)?,
                bytes: counted_bytes(r, 7)?,
            },
            34 => Message::Finalise,
            52 => Message::GroupSatNav {
                key: ObjKey::read(r, LOBBY_KEY_BITS)?,
                node: word(r)? as i32,
            },
            70 => Message::Inactive {
                key: ObjKey::read(r, LOBBY_KEY_BITS)?,
            },
            _ => return Err(Truncated),
        })
    }

    /// A forwarded body: a copy of the reader walks it to its end, then the bits up to there are
    /// taken as received.
    fn decode_opaque(id: u8, r: &mut Rd<'_>) -> Rr<Message> {
        let mut scout = *r;
        walk_forwarded_body(id, &mut scout)?;
        let width = scout.pos() - r.pos();
        Ok(Message::Opaque {
            id,
            body: r.take(width)?,
        })
    }

    fn encode_body(&self, out: &mut Bits) {
        match self {
            Message::Connect {
                version,
                game,
                flags,
                user,
                a,
                b,
                attempt,
            } => {
                out.byte_string(version).byte_string(game);
                out.push(u64::from(*flags), 2).push(*user, 64);
                for value in [a, b, attempt] {
                    out.push(u64::from(*value), 32);
                }
            }
            Message::ConnectReply {
                flags,
                reason,
                attempt,
                own_attempt,
            } => {
                out.push(u64::from(*flags), 2).push(u64::from(*reason), 3);
                for value in [attempt, own_attempt] {
                    out.push(u64::from(*value), 32);
                }
            }
            Message::Disconnect | Message::Finalise => {}
            Message::Ping { clock } | Message::SyncComplete { attempt: clock } => {
                out.push(u64::from(*clock), 32);
            }
            Message::Pong { echoed, clock } => {
                for value in [echoed, clock] {
                    out.push(u64::from(*value), 32);
                }
            }
            Message::ClockSync { round, clock } => {
                out.push(u64::from(*round), 16);
                out.push(u64::from(*clock), 32);
            }
            Message::Object(object) => object.encode_into(out),
            Message::ObjectDelete {
                router_id,
                key,
                sequence,
            } => {
                out.push(u64::from(*router_id), ROUTER_BITS);
                key.write(out, KeyLayout::id_bits_or_fallback(*router_id));
                out.push(u64::from(*sequence), SEQUENCE_BITS);
            }
            Message::CarState { car, body } | Message::CarEvent { car, body, .. } => {
                car.write(out, PLAYER_KEY_BITS);
                out.append(body);
            }
            Message::TagCollision {
                first,
                second,
                values,
            } => {
                for car in [first, second] {
                    car.write(out, PLAYER_KEY_BITS);
                }
                for value in values {
                    out.push(u64::from(*value), 3);
                }
            }
            Message::TagBuckets { cars, buckets } => {
                for car in cars {
                    car.write(out, PLAYER_KEY_BITS);
                }
                for bucket in buckets {
                    out.push(u64::from(*bucket), 8);
                }
            }
            Message::TrafficRelay { body, .. } | Message::Opaque { body, .. } => {
                out.append(body);
            }
            Message::DetachPart { part, bytes } => {
                out.push(u64::from(*part), 4);
                put_counted_bytes(out, bytes, 11);
            }
            Message::Chat { xuid, bytes } => {
                out.push(*xuid, 64);
                put_counted_bytes(out, bytes, 7);
            }
            Message::Inactive { key } => key.write(out, LOBBY_KEY_BITS),
            Message::GroupSatNav { key, node } => {
                key.write(out, LOBBY_KEY_BITS);
                out.push(u64::from(*node as u32), 32);
            }
        }
    }
}

/// Walks a forwarded body to its end: the ownership family with the engine's router rules, every
/// other forwarded message with its registry layout.
fn walk_forwarded_body(id: u8, r: &mut Rd<'_>) -> Rr<()> {
    if !forwards_opaque(id) {
        return Err(Truncated);
    }
    match registry_row(id).map(|row| row.body) {
        Some(BodyKind::Object) => walk_ownership(id, r),
        Some(BodyKind::Fields(layout)) => layout.iter().try_for_each(|field| walk_field(field, r)),
        _ => Err(Truncated),
    }
}

/// The ownership and needs-updates bodies (10-13): the router (hosted only), then
/// - request: the type, the key;
/// - grant: the key, the sequence, a length-prefixed payload;
/// - transfer: the key, the sequence;
/// - needs-updates (console router only): the key, a count, then that many entries.
fn walk_ownership(id: u8, r: &mut Rd<'_>) -> Rr<()> {
    let router_id = r.read(ROUTER_BITS)? as u8;
    let layout = KeyLayout::of(router_id).ok_or(Truncated)?;
    match id {
        MSG_REQUEST_OWNERSHIP => {
            r.skip(layout.type_bits as usize)?;
            ObjKey::read(r, layout.id_bits).map(drop)
        }
        MSG_GRANT_OWNERSHIP => {
            ObjKey::read(r, layout.id_bits)?;
            r.skip(usize::from(SEQ_BITS))?;
            let length = r.read(LENGTH_BITS)? as usize;
            r.skip(length)
        }
        MSG_TRANSFER_OWNERSHIP => {
            ObjKey::read(r, layout.id_bits)?;
            r.skip(usize::from(SEQ_BITS))
        }
        MSG_NEEDS_UPDATES if router_id == NEEDS_UPDATES_ROUTER => {
            ObjKey::read(r, layout.id_bits)?;
            let entries = r.read(u32::from(NEEDS_UPDATES_COUNT_BITS))? as usize;
            r.skip(entries * usize::from(NEEDS_UPDATES_ENTRY_BITS))
        }
        _ => Err(Truncated),
    }
}

/// Steps over one registry field on the engine's reader.
fn walk_field(field: &Field, r: &mut Rd<'_>) -> Rr<()> {
    match *field {
        Field::U(bits) | Field::S(bits) => r.skip(usize::from(bits)),
        Field::Raw(bits) => r.skip(usize::from(bits)),
        Field::Bool => r.skip(1),
        Field::F32 => r.skip(32),
        Field::Vec3 => r.skip(96),
        Field::U64 => r.skip(64),
        Field::Blob64 => r.byte_string(BLOB64_LEN).map(drop),
        Field::Str(max) => r.byte_string(usize::from(max)).map(drop),
        Field::Key(id_bits) => ObjKey::read(r, u32::from(id_bits)).map(drop),
        Field::Repeat { count_bits, item } => {
            let rounds = r.read(u32::from(count_bits))?;
            (0..rounds).try_for_each(|_| item.iter().try_for_each(|inner| walk_field(inner, r)))
        }
    }
}
