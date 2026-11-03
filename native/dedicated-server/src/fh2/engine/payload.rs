// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Reading and rewriting the object payloads the engine forwards, before they reach the other
//! games' trusting deserializers (NERDCORE_WIRE.md §4.5, §7): the player (router 1), car (2),
//! console (3) and bucket-list player state (12) are re-encoded variable by variable; a payload
//! that does not read, repeats a variable or carries a non-finite or out-of-range value is refused.
//! The console's server-owned flags (1, 2, 4) are stripped: only the server writes them.

use std::sync::OnceLock;

use super::{
    bits::{Bits, Rd},
    key::ObjKey,
    message::{CAR_BODY_BITS, ObjectChange, car_event_body_bits},
};
use crate::fh2::codec::synced;

pub const NETWORK_NAME_MAX: u64 = 16;
pub const FLOAT_MAX: f32 = 1e6;
pub const POSITION_MAX: f32 = 1e5;

/// The player object's server-written variables (contract: PrivateUniverseId, TeamId).
pub const PRIVATE_UNIVERSE_VARIABLE: u8 = 40;
pub const PRIVATE_UNIVERSE_BITS: u32 = 5;
pub const TEAM_VARIABLE: u8 = 90;
pub const TEAM_BITS: u32 = 1;
pub const ROAD_TRIP_LEADER_VARIABLE: u8 = 13;
/// BucketListPlayerStateSynced's State, and the highest value its enum has.
pub const BUCKET_LIST_STATE_VARIABLE: usize = 4;
pub const BUCKET_LIST_STATE_MAX: u64 = 7;
pub const BUCKET_LIST_PLAYER_MAX_BITS: usize = 128;

/// Positive = bits, -1 = a finite f32, -2 = a bounded UTF-16 NetworkName.
const PLAYER_VARIABLES: [i32; 12] = [64, 1, 1, -2, 64, 1, 32, 1, 32, 32, 32, 32];
const CAR_VARIABLES: [i32; 22] = [
    -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 5,
];
const CONSOLE_VARIABLES: [i32; 9] = [1, 1, 1, 1, 1, 1, 1, 2, 2];
const BUCKET_LIST_PLAYER_VARIABLES: [i32; 6] = [32, 32, 32, 1, 4, 1];

/// What made the sanitizer refuse a payload, with the number `validation_rejected` reports
/// (the router, the variable id, the value or the payload's length; 0 for a cut payload) and
/// where it happened (the variable id, the router or the length: never a decoded value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refusal {
    pub cause: Cause,
    /// For the controller's `peer.rejected` only (contract §4.5); it may be a decoded value.
    pub detail: u32,
    /// What a world-log note may carry (contract §2.3): a catalog id, a size or a position.
    pub at: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// No variable list for this router.
    UnsanitizedRouter,
    /// A variable id past the object's capacity.
    IdPastCapacity,
    /// The same variable twice in one payload.
    RepeatedId,
    /// A float that is NaN, infinite or past its bound.
    BadFloat,
    /// A NetworkName longer than the game's 16 units.
    LongNetworkName,
    /// The payload ends inside a variable.
    CutShort,
    /// The payload is longer than the object can be.
    Oversized,
    /// An enum or counter past its range.
    PastRange,
}

impl Cause {
    /// The cause as the world log's notes name it: the reference engine's `PayloadFault` names,
    /// which the D2 vectors pin.
    pub fn label(self) -> &'static str {
        match self {
            Cause::UnsanitizedRouter => "Router",
            Cause::IdPastCapacity => "Index",
            Cause::RepeatedId => "Duplicate",
            Cause::BadFloat => "NotFinite",
            Cause::LongNetworkName => "NameTooLong",
            Cause::CutShort => "Truncated",
            Cause::Oversized => "TooBig",
            Cause::PastRange => "OutOfRange",
        }
    }

    fn with(self, detail: impl TryInto<u32>) -> Refusal {
        let detail = detail.try_into().unwrap_or(u32::MAX);
        Refusal {
            cause: self,
            detail,
            at: detail,
        }
    }

    /// A refusal whose number is a decoded value: notes get the variable id instead.
    fn with_value(self, value: impl TryInto<u32>, variable: usize) -> Refusal {
        Refusal {
            at: u32::try_from(variable).unwrap_or(u32::MAX),
            ..self.with(value)
        }
    }
}

/// RoadTripPlayerSynced (router 9, type 2; NERDCORE_WIRE.md §7.9): 0 State (4 bits, 0..8),
/// 1 CompletedEventCount (3 bits, 0..4), 2 XPOnDestinationJoin (s32); 3-bit ids.
pub const ROAD_TRIP_PLAYER_ROUTER: u8 = 9;
const ROAD_TRIP_PLAYER_VARIABLES: [i32; 3] = [4, 3, 32];
const ROAD_TRIP_PLAYER_MAXIMA: [Option<u64>; 3] = [Some(8), Some(4), None];
/// The largest RoadTripPlayerSynced payload relayed.
pub const ROAD_TRIP_PLAYER_MAX_BITS: usize = 256;

/// What the codec's §7 variable list says about routers 1-3's objects: the variable-id width,
/// the capacity (ids below it are legal) and the longest well-formed payload.
#[derive(Debug, Clone, Copy)]
struct SchemaFacts {
    id_bits: u32,
    capacity: usize,
    widest: usize,
}

fn schema_facts(router: u8) -> Option<SchemaFacts> {
    static FACTS: OnceLock<[SchemaFacts; 3]> = OnceLock::new();
    let slot = usize::from(router).checked_sub(1).filter(|i| *i < 3)?;
    let all = FACTS.get_or_init(|| {
        [1u8, 2, 3].map(|r| {
            let schema = synced::object_schema(r, 0).expect("routers 1-3 have a §7 list");
            SchemaFacts {
                id_bits: u32::from(schema.var_id_bits()),
                capacity: schema.count as usize,
                widest: schema.widest_payload_bits(),
            }
        })
    });
    Some(all[slot])
}

/// The longest payload relayed for `router`. The player object keeps room above its widest
/// encoding (rounded up to a power of two) for the unmapped tail it copies as it came; the car
/// and the console are held to theirs exactly.
pub fn max_bits(router: u8) -> Option<usize> {
    match router {
        1 => schema_facts(1).map(|f| f.widest.next_power_of_two()),
        2 | 3 => schema_facts(router).map(|f| f.widest),
        ROAD_TRIP_PLAYER_ROUTER => Some(ROAD_TRIP_PLAYER_MAX_BITS),
        12 => Some(BUCKET_LIST_PLAYER_MAX_BITS),
        _ => None,
    }
}

fn index_width(router: u8) -> u32 {
    match router {
        1..=3 => schema_facts(router).map_or(0, |f| f.id_bits),
        ROAD_TRIP_PLAYER_ROUTER | 12 => 3,
        _ => 0,
    }
}

/// Player variables past the mapped twelve whose width is known (a lone update of one is
/// superseded by a later one of the same variable).
fn player_single_width(index: u32) -> u32 {
    match index {
        12 => 1,
        13 => 1,
        16 => 1,
        19 => 4,
        27 => 32,
        28 => 32,
        32 => 1,
        40 => PRIVATE_UNIVERSE_BITS,
        41 => 64,
        90 => TEAM_BITS,
        _ => 0,
    }
}

/// The variable an update carries when its whole payload is exactly one variable of known width.
pub fn single_variable(router: u8, payload: &Bits) -> Option<u32> {
    let iw = index_width(router);
    if iw == 0 || payload.len() <= iw as usize {
        return None;
    }
    let index = payload.reader().read(iw).ok()? as u32;
    let i = index as usize;
    let width: i64 = match router {
        1 if i < PLAYER_VARIABLES.len() => i64::from(PLAYER_VARIABLES[i]),
        1 => i64::from(player_single_width(index)),
        2 if i < CAR_VARIABLES.len() => {
            if CAR_VARIABLES[i] == -1 {
                32
            } else {
                i64::from(CAR_VARIABLES[i])
            }
        }
        3 if i < CONSOLE_VARIABLES.len() => i64::from(CONSOLE_VARIABLES[i]),
        12 if i < BUCKET_LIST_PLAYER_VARIABLES.len() => i64::from(BUCKET_LIST_PLAYER_VARIABLES[i]),
        _ => 0,
    };
    (width > 0 && payload.len() as i64 == i64::from(iw) + width).then_some(index)
}

pub fn float_ok(bits: u32, maximum: f32) -> bool {
    let n = f32::from_bits(bits);
    n.is_finite() && n.abs() <= maximum
}

/// The re-encoded payload, or why it was refused.
pub fn sanitize(router: u8, payload: &Bits) -> Result<Bits, Refusal> {
    let variables: &[i32] = match router {
        1 => &PLAYER_VARIABLES,
        2 => &CAR_VARIABLES,
        3 => &CONSOLE_VARIABLES,
        ROAD_TRIP_PLAYER_ROUTER => &ROAD_TRIP_PLAYER_VARIABLES,
        12 => &BUCKET_LIST_PLAYER_VARIABLES,
        _ => return Err(Cause::UnsanitizedRouter.with(router)),
    };
    if max_bits(router).is_some_and(|m| payload.len() > m) {
        return Err(Cause::Oversized.with(payload.len()));
    }
    let iw = index_width(router);
    let room = match router {
        1 => schema_facts(1).map_or(variables.len(), |f| f.capacity),
        _ => variables.len(),
    };
    let mut r = payload.reader();
    let mut clean = Bits::new();
    let mut written = 0u32;
    let t = |_| Cause::CutShort.with(0u32);
    while r.left() > 0 {
        let start = r;
        let index = r.read(iw).map_err(t)? as usize;
        if index >= variables.len() {
            if index >= room {
                return Err(Cause::IdPastCapacity.with(index));
            }
            // the player object's unmapped tail is kept within its size limit, but for TeamId
            // (var 90), which only the server writes
            player_tail(start, &mut clean);
            return Ok(clean);
        }
        if written & (1 << index) != 0 {
            return Err(Cause::RepeatedId.with(index));
        }
        written |= 1 << index;
        let mut variable = Bits::new();
        variable.push(index as u64, iw);
        match variables[index] {
            -1 => {
                let bits = r.read(32).map_err(t)? as u32;
                if !float_ok(bits, FLOAT_MAX) {
                    return Err(Cause::BadFloat.with(index));
                }
                variable.push(u64::from(bits), 32);
            }
            -2 => {
                let count = r.read(5).map_err(t)?;
                if count > NETWORK_NAME_MAX {
                    return Err(Cause::LongNetworkName.with(count));
                }
                variable.push(count, 5);
                for _ in 0..count {
                    variable.push(r.read(16).map_err(t)?, 16);
                }
            }
            w => {
                let value = r.read(w as u32).map_err(t)?;
                if router == 12
                    && index == BUCKET_LIST_STATE_VARIABLE
                    && value > BUCKET_LIST_STATE_MAX
                {
                    return Err(Cause::PastRange.with_value(value, index));
                }
                // a road-trip player State past RoadTripServerState, a CompletedEventCount past
                // its range
                if router == ROAD_TRIP_PLAYER_ROUTER
                    && ROAD_TRIP_PLAYER_MAXIMA[index].is_some_and(|m| value > m)
                {
                    return Err(Cause::PastRange.with_value(value, index));
                }
                variable.push(value, w as u32);
            }
        }
        // the console's server-owned flags are dropped, every other variable kept
        if router != 3 || !matches!(index, 1 | 2 | 4) {
            clean.append(&variable);
        }
    }
    Ok(clean)
}

/// The player object's unmapped tail (ids from 12): copied, but for every TeamId entry, while
/// each variable's width is known; from the first variable of unknown width (or a cut entry) the
/// rest is copied as it came. True when the whole tail was read.
pub fn player_tail(mut tail: Rd<'_>, out: &mut Bits) -> bool {
    while tail.left() > 0 {
        let start = tail;
        let copy_rest = |start: Rd<'_>, out: &mut Bits| {
            let mut s = start;
            let n = s.left();
            if let Ok(rest) = s.take(n) {
                out.append(&rest);
            }
        };
        if tail.left() < 8 {
            copy_rest(start, out);
            return false;
        }
        let index = tail.read(8).unwrap_or(0) as u32;
        let width = player_single_width(index);
        if (index as usize) < PLAYER_VARIABLES.len() || width == 0 || tail.left() < width as usize {
            copy_rest(start, out);
            return false;
        }
        let value = tail.read(width).unwrap_or(0);
        if index == u32::from(TEAM_VARIABLE) {
            continue; // a client never writes its team
        }
        out.push(u64::from(index), 8).push(value, width);
    }
    true
}

/// The console variables of a payload, in order; `None` when it does not read.
pub fn console_variables(payload: &Bits) -> Option<Vec<(u8, u64)>> {
    let mut out = Vec::new();
    let mut r = payload.reader();
    while r.left() > 0 {
        let index = r.read(4).ok()? as usize;
        if index >= CONSOLE_VARIABLES.len() {
            return None;
        }
        out.push((index as u8, r.read(CONSOLE_VARIABLES[index] as u32).ok()?));
    }
    Some(out)
}

/// The floats a car message's body leads with, once vetted (unused slots are zero).
pub type CarFloats = [f32; 9];

/// How the engine vets a relayed car message body (15: the car state; 20, 21, 35: car events):
/// its registered width, the bits ahead of its floats, how many floats, and their bound.
fn car_layout(message: u8) -> Option<(usize, usize, usize, f32)> {
    let (skip, floats, bound) = match message {
        15 => (0, 3, POSITION_MAX),
        20 => (32, 9, FLOAT_MAX),
        21 => (0, 6, FLOAT_MAX),
        35 => (0, 0, FLOAT_MAX),
        _ => return None,
    };
    let width = match message {
        15 => CAR_BODY_BITS,
        _ => car_event_body_bits(message)?,
    };
    Some((width, skip, floats, bound))
}

/// Vets a car message body: `Some` with its leading floats when the body has the registered width
/// and every float is finite within its bound (a car state's are its position), `None` refuses it.
pub fn vet_car_body(message: u8, body: &Bits) -> Option<CarFloats> {
    let (width, skip, floats, bound) = car_layout(message)?;
    (body.len() == width).then_some(())?;
    let mut cursor = body.reader();
    cursor.skip(skip).ok()?;
    let mut vetted: CarFloats = [0.0; 9];
    for slot in vetted.iter_mut().take(floats) {
        let raw = cursor.read(32).ok()? as u32;
        float_ok(raw, bound).then_some(())?;
        *slot = f32::from_bits(raw);
    }
    Some(vetted)
}

/// SessionHostSynced variables (4-bit ids), each at its width.
pub fn session_host(entries: &[(u8, u64, u32)]) -> Bits {
    let mut b = Bits::new();
    for (var, value, width) in entries {
        b.push(u64::from(*var), 4).push(*value, *width);
    }
    b
}

pub fn group_sat_nav(node: i32) -> Bits {
    session_host(&[(12, u64::from(node as u32), 32)])
}

/// The server's update of a console's flags (a server key).
pub fn console_update(id: u32, seq: u8, entries: &[(u8, bool)]) -> ObjectChange {
    let mut payload = Bits::new();
    for (var, value) in entries {
        payload.push(u64::from(*var), 4).boolean(*value);
    }
    ObjectChange {
        is_create: false,
        router_id: 3,
        key: ObjKey::server(id),
        ty: 0,
        sequence: seq,
        vars: payload,
    }
}

/// BucketListSlotArraySynced variables (6-bit ids, s32 values).
pub fn bucket_list_slots(vars: &[(u8, i32)]) -> Bits {
    let mut b = Bits::new();
    for (var, value) in vars {
        b.push(u64::from(*var), 6)
            .push(u64::from(*value as u32), 32);
    }
    b
}

/// BucketListPlayerStateSynced's CurrentSlotId, CurrentChallengeId and RequestingPartner.
pub const BUCKET_LIST_SLOT_VARIABLE: u8 = 1;
pub const BUCKET_LIST_CHALLENGE_VARIABLE: u8 = 2;
pub const BUCKET_LIST_REQUESTING_VARIABLE: u8 = 3;

/// A BucketListPlayerStateSynced payload's variables, in order. A payload that does not read
/// fully gives its clean prefix: the variables before the first unknown or repeated id, cut value
/// or State past the enum.
pub fn bucket_list_player_of(payload: &Bits) -> Vec<(u8, u64)> {
    let mut out = Vec::new();
    let mut r = payload.reader();
    let mut seen = 0u32;
    while r.left() > 0 {
        let Ok(index) = r.read(3) else { break };
        let index = index as usize;
        if index >= BUCKET_LIST_PLAYER_VARIABLES.len() || seen & (1 << index) != 0 {
            break;
        }
        let Ok(value) = r.read(BUCKET_LIST_PLAYER_VARIABLES[index] as u32) else {
            break;
        };
        if index == BUCKET_LIST_STATE_VARIABLE && value > BUCKET_LIST_STATE_MAX {
            break;
        }
        seen |= 1 << index;
        out.push((index as u8, value));
    }
    out
}

/// BucketListPlayerStateSynced variables re-encoded in the game's layout (3-bit ids, the pinned
/// widths).
pub fn bucket_list_player(vars: &[(u8, u64)]) -> Bits {
    let mut b = Bits::new();
    for (var, value) in vars {
        b.push(u64::from(*var), 3)
            .push(*value, BUCKET_LIST_PLAYER_VARIABLES[*var as usize] as u32);
    }
    b
}

/// The player object's NetworkName (var 3) in one relayed payload, the last one it carries (UTF-16
/// units), read from the payload's start while the variables' widths are known; `None`: none.
pub fn player_name(payload: &Bits) -> Option<Vec<u16>> {
    let mut r = payload.reader();
    let mut name = None;
    while r.left() >= 8 {
        let Ok(index) = r.read(8) else { break };
        let index = index as u32;
        let i = index as usize;
        if i >= PLAYER_VARIABLES.len() {
            let width = player_single_width(index);
            if width == 0 || r.skip(width as usize).is_err() {
                return name;
            }
            continue;
        }
        let width = PLAYER_VARIABLES[i];
        if width != -2 {
            // the mapped twelve have fixed widths but the name (no float among them)
            if r.skip(width as usize).is_err() {
                return name;
            }
            continue;
        }
        let Ok(count) = r.read(5) else { return name };
        let mut units = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let Ok(u) = r.read(16) else { return name };
            units.push(u as u16);
        }
        name = Some(units);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_carries_no_decoded_value_where_notes_read() {
        // an out-of-range value: the value for peer.rejected, the variable for notes
        let mut p = Bits::new();
        p.push(BUCKET_LIST_STATE_VARIABLE as u64, 3).push(12, 4);
        let r = sanitize(12, &p).unwrap_err();
        assert_eq!((r.cause, r.detail, r.at), (Cause::PastRange, 12, 4));
        // ids, sizes and positions are the same for both
        let mut repeated = Bits::new();
        repeated.push(1, 4).push(1, 1).push(1, 4).push(0, 1);
        let r = sanitize(3, &repeated).unwrap_err();
        assert_eq!((r.cause, r.detail, r.at), (Cause::RepeatedId, 1, 1));
    }
}
