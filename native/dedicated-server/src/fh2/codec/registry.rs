// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written from docs/launcher/NERDCORE_WIRE.md (OD-187); no other codec source used.
//! The message-body registry keyed by the 7-bit message id (§2.4, §6.3).
//!
//! [`Registry::fh2`] holds the 77 ids of the game's table. A message whose body the spec gives
//! is a [`BodyKind::Fields`] layout (or [`BodyKind::Object`] for 7-9); one the spec does not give
//! yet is [`BodyKind::Unparsed`], which makes the packet reader stop and keep the rest of the
//! packet raw (§2.4: a receiver must parse every message or lose its place). New bodies are
//! added with [`Registry::register`], including hand-written [`BodyKind::Custom`] codecs for
//! nested readers.

use super::{
    CodecError,
    bits::{BitReader, BitWriter},
    fields::{Field, FieldValue, read_fields, write_fields},
    object::ObjectMessage,
};

/// The message-id width (§2.4).
pub const MESSAGE_ID_BITS: u8 = 7;
/// The highest valid message id (§2.4).
pub const MAX_MESSAGE_ID: u8 = 76;
/// In connect-only mode only ids `0..=6` are accepted (§2.4).
pub const MAX_CONNECT_ONLY_ID: u8 = 6;
/// Messages of a class below this are resent when their packet is lost (§3.1).
pub const RELIABLE_CLASS_LIMIT: u8 = 3;

pub type CustomDecode = fn(&mut BitReader<'_>) -> Result<Vec<FieldValue>, CodecError>;
pub type CustomEncode = fn(&[FieldValue], &mut BitWriter) -> Result<(), CodecError>;

/// How a message's body is coded.
#[derive(Debug, Clone, Copy)]
pub enum BodyKind {
    /// A field list in the notation of §6.2.
    Fields(&'static [Field]),
    /// An object message (ids 7-9, §4.4).
    Object,
    /// A hand-written codec (for bodies a field list cannot express).
    Custom {
        decode: CustomDecode,
        encode: CustomEncode,
    },
    /// Not decodable yet; the reason names the spec gap. The packet reader discards the rest of
    /// the packet at such a message.
    Unparsed(&'static str),
}

/// Who sends a message (§6.3's two body columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The game reads and writes it.
    Both,
    /// Only the game reads it (a server writes it).
    ToGame,
    /// Only the game writes it (a server parses it).
    FromGame,
    /// Neither handled nor sent by the game.
    Neither,
}

/// One row of §6.3.
#[derive(Debug, Clone, Copy)]
pub struct MessageSpec {
    pub id: u8,
    pub name: &'static str,
    /// The class the game's writer passes (§3.1), when the game sends the message.
    pub sender_class: Option<u8>,
    /// The number in the game's table (§6.1).
    pub table_value: u8,
    /// The router it is registered under, `None` for core and unrouted messages.
    pub router: Option<u8>,
    /// The leading key is read by a routed handler: an unknown object makes the game skip the
    /// body and go on with the packet (§6.2).
    pub routed: bool,
    pub direction: Direction,
    pub body: BodyKind,
}

impl MessageSpec {
    /// The class a server should send this message with: the game's sender class when it has
    /// one (§3.1 correction), else the table value (§6.2: "the only hint").
    pub fn send_class(&self) -> u8 {
        self.sender_class.unwrap_or(self.table_value)
    }

    /// Whether a lost packet re-queues this message (§3.1).
    pub fn is_reliable(&self) -> bool {
        self.send_class() < RELIABLE_CLASS_LIMIT
    }
}

/// A decoded message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Object(ObjectMessage),
    Fields(Vec<FieldValue>),
}

/// The registry: one optional spec per id `0..=76`.
#[derive(Debug, Clone)]
pub struct Registry {
    specs: Vec<Option<MessageSpec>>,
}

impl Default for Registry {
    fn default() -> Registry {
        Registry::fh2()
    }
}

impl Registry {
    /// No message registered.
    pub fn empty() -> Registry {
        Registry {
            specs: vec![None; usize::from(MAX_MESSAGE_ID) + 1],
        }
    }

    /// Forza Horizon 2's 77 messages (§6.3).
    pub fn fh2() -> Registry {
        let mut r = Registry::empty();
        for s in FH2_MESSAGES {
            r.specs[usize::from(s.id)] = Some(*s);
        }
        r
    }

    /// Adds or replaces the spec of `spec.id`; `Err(MessageId)` above 76.
    pub fn register(&mut self, spec: MessageSpec) -> Result<(), CodecError> {
        let slot = self
            .specs
            .get_mut(usize::from(spec.id))
            .ok_or(CodecError::MessageId(spec.id))?;
        *slot = Some(spec);
        Ok(())
    }

    pub fn get(&self, id: u8) -> Option<&MessageSpec> {
        self.specs.get(usize::from(id)).and_then(Option::as_ref)
    }

    /// Reads the body of message `id` (its 7-bit id already read).
    pub fn read_body(&self, id: u8, r: &mut BitReader<'_>) -> Result<Body, CodecError> {
        if id > MAX_MESSAGE_ID {
            return Err(CodecError::MessageId(id));
        }
        let spec = self.get(id).ok_or(CodecError::Unparsed(id))?;
        match spec.body {
            BodyKind::Fields(layout) => Ok(Body::Fields(read_fields(layout, r)?)),
            BodyKind::Object => Ok(Body::Object(ObjectMessage::read(id, r)?)),
            BodyKind::Custom { decode, .. } => Ok(Body::Fields(decode(r)?)),
            BodyKind::Unparsed(_) => Err(CodecError::Unparsed(id)),
        }
    }

    /// Writes the body of message `id` (without the 7-bit id).
    pub fn write_body(&self, id: u8, body: &Body, w: &mut BitWriter) -> Result<(), CodecError> {
        let spec = self.get(id).ok_or(CodecError::Unparsed(id))?;
        match (spec.body, body) {
            (BodyKind::Fields(layout), Body::Fields(v)) => write_fields(layout, v, w),
            (BodyKind::Object, Body::Object(m)) if m.message_id() == id => m.write(w),
            (BodyKind::Custom { encode, .. }, Body::Fields(v)) => encode(v, w),
            (BodyKind::Unparsed(_), _) => Err(CodecError::Unparsed(id)),
            _ => Err(CodecError::Layout),
        }
    }
}

use Direction::{Both, FromGame, Neither, ToGame};
use Field::{Blob64 as B64, Bool, F32, Key, S, Str, U, U64, Vec3};

#[allow(clippy::too_many_arguments)]
const fn m(
    id: u8,
    name: &'static str,
    sender_class: Option<u8>,
    table_value: u8,
    router: Option<u8>,
    routed: bool,
    direction: Direction,
    body: BodyKind,
) -> MessageSpec {
    MessageSpec {
        id,
        name,
        sender_class,
        table_value,
        router,
        routed,
        direction,
        body,
    }
}

const fn f(layout: &'static [Field]) -> BodyKind {
    BodyKind::Fields(layout)
}

const EMPTY: BodyKind = BodyKind::Fields(&[]);
const S16X9: [Field; 9] = [S(16); 9];

/// `[f32 f32 f32 s32 s32 s32 s32]` (NewAtmosphericData's block).
const ATMOS: [Field; 7] = [F32, F32, F32, S(32), S(32), S(32), S(32)];
/// `[f32 f32 f32]` (NewPrecipitationData's block).
const PRECIP: [Field; 3] = [F32, F32, F32];

#[rustfmt::skip]
const CAR_PREDICTION_FULL: &[Field] = &[
    Key(7),
    Vec3,
    S16X9[0], S16X9[1], S16X9[2], S16X9[3], S16X9[4], S16X9[5], S16X9[6], S16X9[7], S16X9[8],
    U(16), S(8), S(16), S(8), U(8), U(8), S(8), S(8), U(8), U(8), U(8), U(8), U(8), S(8), U(8),
    U(8), U(8), U(2), U(1), U(5), U(4), U(4), U(4), U(4), U(4), U(1), U(1),
    U(32),
];
#[rustfmt::skip]
const TRAFFIC_FULL_SYNC: &[Field] = &[
    U(8),
    U(32), Vec3,
    S16X9[0], S16X9[1], S16X9[2], S16X9[3], S16X9[4], S16X9[5], S16X9[6], S16X9[7], S16X9[8],
    U(32), Bool, Bool,
];
const TRAFFIC_ROUTE_PROGRESS: &[Field] = &[U(8), F32, U(32), U(32), Vec3];
const TRAFFIC_MESSAGE: &[Field] = &[U(4), U(8), S(14), U(5), U(8), Bool, S(32)];
#[rustfmt::skip]
const TRAFFIC_FULL_ROUTE: &[Field] = &[
    U(8),
    S(16), S(16), S(16), S(16), S(16), S(16), S(16), S(16), U(2),
    F32, U(32), U(32),
    Vec3,
];
const TRAFFIC_STATIONARY: &[Field] = &[U(8), Vec3, S(16), S(16), S(16), U(32), Bool];
#[rustfmt::skip]
const FULL_WEATHER: &[Field] = &[
    ATMOS[0], ATMOS[1], ATMOS[2], ATMOS[3], ATMOS[4], ATMOS[5], ATMOS[6],
    PRECIP[0], PRECIP[1], PRECIP[2],
    S(32), S(32),
];
const KEY7_ITEM: &[Field] = &[Key(7)];
const BYTE_ITEM: &[Field] = &[U(8)];

/// §6.3, one row per id.
#[rustfmt::skip]
pub const FH2_MESSAGES: &[MessageSpec] = &[
    m(0, "OpenConnection", Some(2), 2, None, false, Both,
        f(&[Str(38), Str(16), U(2), U64, S(32), S(32), U(32)])),
    m(1, "OpenConnectionResponse", Some(2), 2, None, false, Both, f(&[U(2), U(3), U(32), U(32)])),
    m(2, "CloseConnection", Some(2), 4, None, false, Both, f(&[U(3), U(32), U(32)])),
    m(3, "Ping", Some(4), 4, None, false, Both, f(&[S(32)])),
    m(4, "Pong", Some(4), 5, None, false, Both, f(&[S(32), S(32)])),
    m(5, "TimeSync", None, 2, None, false, ToGame, f(&[U(16), U(32)])),
    m(6, "ConnectionSyncDone", None, 1, None, false, ToGame, f(&[U(32)])),
    m(7, "ObjectCreate", None, 1, None, false, Both, BodyKind::Object),
    m(8, "ObjectUpdate", None, 1, None, false, Both, BodyKind::Object),
    m(9, "ObjectDelete", None, 1, None, false, Both, BodyKind::Object),
    // SPEC: §4.4 does not decode the ownership/needs-updates bodies (10-13).
    m(10, "ObjectRequestOwnership", None, 1, None, false, Both,
        BodyKind::Unparsed("§4.4: ownership body not decoded")),
    m(11, "ObjectGrantOwnership", None, 1, None, false, Both,
        BodyKind::Unparsed("§4.4: ownership body not decoded")),
    m(12, "ObjectTransferOwnership", None, 1, None, false, Both,
        BodyKind::Unparsed("§4.4: ownership body not decoded")),
    m(13, "ObjectNeedsUpdates", None, 4, None, false, Both,
        BodyKind::Unparsed("§4.4: needs-updates body not decoded")),
    m(14, "PlayerInput", None, 4, Some(1), true, ToGame,
        f(&[Key(7), U(16), S(12), S(12), Bool, Bool, Bool, Bool])),
    m(15, "CarPredictionFull", Some(4), 4, Some(2), true, Both, f(CAR_PREDICTION_FULL)),
    // SPEC: unverified (§6.3): the leading u8 of the traffic messages is coded as a single field
    // followed by one nested block, as the table writes it (not as a loop count).
    m(16, "ServerTrafficFullSync", Some(4), 4, None, false, FromGame, f(TRAFFIC_FULL_SYNC)),
    m(17, "ServerTrafficRouteProgressUpdate", Some(4), 4, None, false, FromGame,
        f(TRAFFIC_ROUTE_PROGRESS)),
    m(18, "ClientTrafficFullSync", None, 4, Some(8), false, ToGame, f(TRAFFIC_FULL_SYNC)),
    // SPEC: §6.3 shows only the first field of 19 and 75 (nested readers not expanded). The
    // matching ServerTraffic layouts are not assumed.
    m(19, "ClientTrafficRouteProgressUpdate", None, 5, Some(8), false, ToGame,
        BodyKind::Unparsed("§6.3: nested reader not expanded")),
    m(20, "MsgBreakGlass", Some(5), 5, Some(2), false, Both,
        f(&[Key(7), U(32), Vec3, Vec3, Vec3, S(32)])),
    m(21, "MsgCarCollision", Some(5), 5, Some(2), false, Both, f(&[Key(7), Vec3, F32, F32, F32])),
    // SPEC: 22-25 belong to a collision-object handler outside the routers (§6.3); left
    // unparsed until that handler is specified.
    m(22, "MsgCollObjPos", None, 5, None, false, Neither,
        BodyKind::Unparsed("§6.3: not handled, not sent")),
    m(23, "MsgCollObjRest", None, 5, None, false, ToGame,
        BodyKind::Unparsed("§6.3: collision-object handler")),
    m(24, "MsgCollObjDetachPart", Some(5), 5, None, false, Both,
        BodyKind::Unparsed("§6.3: collision-object handler")),
    m(25, "MsgCollObjPosVel", None, 0, None, false, ToGame,
        BodyKind::Unparsed("§6.3: collision-object handler")),
    m(26, "MsgCarMeetPlayerInviteRequest", Some(0), 0, Some(11), false, Both, f(&[U(2), U(32)])),
    m(27, "MsgCarMeetPlayerJoinOrCancelRequest", Some(0), 0, Some(11), false, Both,
        f(&[Bool, U(2), U(32)])),
    m(28, "MsgCarMeetLaunchWagerRace", None, 0, Some(11), false, ToGame, EMPTY),
    m(29, "MsgCarMeetNotifyPendingGameMode", None, 0, Some(11), false, ToGame, f(&[U(2)])),
    m(30, "MsgCarMeetConfigureGameOptionsForWagerRace", None, 0, Some(11), false, ToGame, EMPTY),
    m(31, "MsgCarMeetConfiguredGameOptionsForWagerRace", Some(0), 0, Some(11), false, Both,
        f(&[U(32)])),
    m(32, "MsgCarMeetGameModeSessionReady", Some(0), 0, Some(11), false, Both,
        f(&[Str(64), Str(64)])),
    m(33, "MsgChatManagement", Some(0), 2, Some(3), false, Both,
        f(&[U64, Field::Repeat { count_bits: 7, item: BYTE_ITEM }])),
    m(34, "FinaliseSessionEntry", Some(2), 2, Some(3), false, Both, EMPTY),
    m(35, "MsgRepairDamage", Some(2), 2, Some(2), false, Both, f(&[Key(7)])),
    m(36, "MsgTagCollision", Some(2), 2, Some(2), false, Both,
        f(&[Key(7), Key(7), U(3), U(3), U(3), U(3)])),
    // SPEC: §6.3 writes 37 as "{loop } key(7)×12 u8×12" with no count before the loop; left
    // unparsed until the loop is resolved.
    m(37, "MsgTagBuckets", None, 2, Some(2), false, ToGame,
        BodyKind::Unparsed("§6.3: loop without a count")),
    m(38, "MsgRequestRace", Some(2), 2, Some(6), false, Both, EMPTY),
    // SPEC: unverified (§6.3/§6.4): the table shows a stray "{loop }" before the u4; coded as
    // §6.4 describes it: a 4-bit count, then that many 7-bit keys.
    m(39, "MsgStartingGrid", None, 2, Some(6), true, ToGame,
        f(&[Key(2), Field::Repeat { count_bits: 4, item: KEY7_ITEM }])),
    m(40, "MsgStartRace", None, 2, Some(6), true, ToGame, f(&[Key(2), U(32)])),
    m(41, "MsgReadyRequest", Some(2), 2, Some(6), true, Both, f(&[Key(2), Bool])),
    m(42, "MsgReadyRequestResponse", None, 2, Some(6), true, ToGame, f(&[Key(2), U(2)])),
    m(43, "MsgInProgressScoreUpdate", Some(2), 2, Some(1), true, Both, f(&[Key(7), F32, Bool])),
    m(44, "MsgLapScore", Some(2), 2, Some(1), true, Both,
        f(&[Key(7), B64, B64, B64, U(32), Bool, Bool])),
    m(45, "MsgScoreKeyframe", Some(2), 2, Some(1), true, Both,
        f(&[Key(7), B64, B64, B64, B64, B64, U(32), U(8), U(8), U(8), Bool, Bool, Bool])),
    m(46, "MsgVoteToKick", Some(2), 2, Some(6), true, Both, f(&[Key(2), U64])),
    m(47, "MsgVoteForTrack", Some(2), 2, Some(6), true, Both, f(&[Key(2), S(32)])),
    m(48, "SelectNextRoadTripEvent", None, 2, Some(9), false, ToGame, f(&[S(32), F32, Bool])),
    m(49, "SelectedNextRoadTripEvent", Some(2), 2, Some(9), false, Both, f(&[Bool])),
    m(50, "ConfigureGameOptionsForRoadTripEvent", None, 2, Some(9), false, ToGame,
        f(&[S(32), S(32)])),
    m(51, "ConfiguredGameOptionsForRoadTripEvent", Some(2), 2, Some(9), false, Both,
        f(&[S(32), U(16)])),
    m(52, "SetGroupSatNavEvent", Some(2), 2, Some(6), true, Both, f(&[Key(2), S(32)])),
    m(53, "RequestSendFullWeather", None, 2, Some(10), false, ToGame, EMPTY),
    m(54, "FullWeather", Some(2), 2, Some(10), false, Both, f(FULL_WEATHER)),
    m(55, "RequestSendNewAtmosphericData", None, 2, Some(10), false, ToGame, EMPTY),
    m(56, "NewAtmosphericData", Some(2), 2, Some(10), false, Both, f(&ATMOS)),
    m(57, "RequestSendNewPrecipitationData", None, 2, Some(10), false, ToGame, EMPTY),
    m(58, "NewPrecipitationData", Some(2), 2, Some(10), false, Both, f(&PRECIP)),
    m(59, "RequestLeaderChange", Some(2), 2, Some(1), false, Both, f(&[Key(7)])),
    m(60, "DropLeadership", Some(2), 2, Some(1), false, Both, EMPTY),
    m(61, "BucketListChallengeStart", None, 2, Some(12), false, ToGame, f(&[U(32)])),
    m(62, "BucketListChallengeEnded", Some(2), 2, Some(12), false, Both, f(&[Bool])),
    m(63, "RequestSynchronizeBucketListChallenge", Some(2), 2, Some(12), false, Both,
        f(&[S(32), S(32)])),
    m(64, "RequestDesynchronizeBucketListChallenge", Some(2), 2, Some(12), false, Both, EMPTY),
    m(65, "RespondBucketListPartner", None, 2, Some(12), false, ToGame,
        f(&[Key(7), U(5), Bool, Str(36)])),
    m(66, "BucketListStatusPlayerWaiting", None, 2, Some(12), false, ToGame, f(&[Key(7), S(32)])),
    m(67, "BucketListStatusChallengeStarted", None, 2, Some(12), false, ToGame,
        f(&[Key(7), Key(7), S(32)])),
    m(68, "BucketListStatusChallengeEnded", None, 2, Some(12), false, ToGame,
        f(&[Key(7), Key(7), S(32), Bool])),
    m(69, "MsgDebugLog", None, 2, Some(3), false, ToGame, f(&[U(2)])),
    m(70, "MsgInactivePlayersPulled", Some(2), 1, Some(6), true, Both, f(&[Key(2)])),
    m(71, "ServerTrafficMessage", Some(1), 1, None, false, FromGame, f(TRAFFIC_MESSAGE)),
    m(72, "ClientTrafficMessage", None, 1, Some(8), false, ToGame, f(TRAFFIC_MESSAGE)),
    m(73, "ServerTrafficFullRoute", Some(1), 1, None, false, FromGame, f(TRAFFIC_FULL_ROUTE)),
    m(74, "ServerTrafficStationary", Some(1), 1, None, false, FromGame, f(TRAFFIC_STATIONARY)),
    m(75, "ClientTrafficFullRoute", None, 1, Some(8), false, ToGame,
        BodyKind::Unparsed("§6.3: nested reader not expanded")),
    m(76, "ClientTrafficStationary", None, 1, Some(8), false, ToGame, f(TRAFFIC_STATIONARY)),
];
