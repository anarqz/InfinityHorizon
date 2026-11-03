// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Reading and rewriting the object payloads the engine forwards, before they reach the other
//! games' trusting deserializers (NERDCORE_WIRE.md §4.5, §7): the player (router 1), car (2),
//! console (3) and bucket-list player state (12) are re-encoded variable by variable; a payload
//! that does not read, repeats a variable or carries a non-finite or out-of-range value is refused.
//! The console's server-owned flags (1, 2, 4) are stripped: only the server writes them.

use super::{
    bits::{Bits, Rd},
    wire::{CAR_BODY_BITS, Handle, Obj},
};

pub const NETWORK_NAME_MAX: u64 = 16;
pub const PLAYER_DATA_MAX_BITS: usize = 8192;
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

/// Why a payload was refused (`validation_rejected`'s value is its number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Router(u32),
    Index(u32),
    Duplicate(u32),
    NotFinite(u32),
    NameTooLong(u32),
    Truncated,
    TooBig(u32),
    OutOfRange(u32),
}

impl Fault {
    pub fn value(&self) -> u32 {
        match *self {
            Fault::Router(v)
            | Fault::Index(v)
            | Fault::Duplicate(v)
            | Fault::NotFinite(v)
            | Fault::NameTooLong(v)
            | Fault::TooBig(v)
            | Fault::OutOfRange(v) => v,
            Fault::Truncated => 0,
        }
    }
}

pub fn max_bits(router: u8) -> Option<usize> {
    match router {
        1 => Some(PLAYER_DATA_MAX_BITS),
        2 => Some(787),
        3 => Some(47),
        12 => Some(BUCKET_LIST_PLAYER_MAX_BITS),
        _ => None,
    }
}

fn index_width(router: u8) -> u32 {
    match router {
        1 => 8,
        2 => 5,
        3 => 4,
        12 => 3,
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

/// The re-encoded payload, or the fault that refused it.
pub fn sanitize(router: u8, payload: &Bits) -> Result<Bits, Fault> {
    let variables: &[i32] = match router {
        1 => &PLAYER_VARIABLES,
        2 => &CAR_VARIABLES,
        3 => &CONSOLE_VARIABLES,
        12 => &BUCKET_LIST_PLAYER_VARIABLES,
        _ => return Err(Fault::Router(u32::from(router))),
    };
    if max_bits(router).is_some_and(|m| payload.len() > m) {
        return Err(Fault::TooBig(payload.len() as u32));
    }
    let iw = index_width(router);
    let room = if router == 1 { 163 } else { variables.len() };
    let mut r = payload.reader();
    let mut out = Bits::new();
    let mut seen = [false; 32];
    let t = |_| Fault::Truncated;
    while r.left() > 0 {
        let start = r;
        let index = r.read(iw).map_err(t)? as usize;
        if index >= variables.len() {
            if index >= room {
                return Err(Fault::Index(index as u32));
            }
            // the player object's unmapped tail is kept within its size limit, but for TeamId
            // (var 90), which only the server writes
            player_tail(start, &mut out);
            return Ok(out);
        }
        if seen[index] {
            return Err(Fault::Duplicate(index as u32));
        }
        seen[index] = true;
        let mut entry = Bits::new();
        entry.push(index as u64, iw);
        match variables[index] {
            -1 => {
                let bits = r.read(32).map_err(t)? as u32;
                if !float_ok(bits, FLOAT_MAX) {
                    return Err(Fault::NotFinite(index as u32));
                }
                entry.push(u64::from(bits), 32);
            }
            -2 => {
                let count = r.read(5).map_err(t)?;
                if count > NETWORK_NAME_MAX {
                    return Err(Fault::NameTooLong(count as u32));
                }
                entry.push(count, 5);
                for _ in 0..count {
                    entry.push(r.read(16).map_err(t)?, 16);
                }
            }
            w => {
                let value = r.read(w as u32).map_err(t)?;
                if router == 12
                    && index == BUCKET_LIST_STATE_VARIABLE
                    && value > BUCKET_LIST_STATE_MAX
                {
                    return Err(Fault::OutOfRange(value as u32));
                }
                entry.push(value, w as u32);
            }
        }
        if router != 3 || !matches!(index, 1 | 2 | 4) {
            out.append(&entry);
        }
    }
    Ok(out)
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

/// A car prediction's position when its body is whole and the position is finite and bounded.
pub fn car_position(body: &Bits) -> Option<[f32; 3]> {
    if body.len() != CAR_BODY_BITS {
        return None;
    }
    let mut r = body.reader();
    let x = r.read(32).ok()? as u32;
    let y = r.read(32).ok()? as u32;
    let z = r.read(32).ok()? as u32;
    (float_ok(x, POSITION_MAX) && float_ok(y, POSITION_MAX) && float_ok(z, POSITION_MAX))
        .then(|| [f32::from_bits(x), f32::from_bits(y), f32::from_bits(z)])
}

pub fn car_event_ok(id: u8, body: &Bits) -> bool {
    if super::wire::car_event_bits(id) != Some(body.len()) {
        return false;
    }
    let mut r = body.reader();
    if id == 20 && r.skip(32).is_err() {
        return false;
    }
    let count = match id {
        20 => 9,
        21 => 6,
        _ => 0,
    };
    (0..count).all(|_| r.read(32).is_ok_and(|b| float_ok(b as u32, FLOAT_MAX)))
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
pub fn console_update(id: u32, seq: u8, entries: &[(u8, bool)]) -> Obj {
    let mut payload = Bits::new();
    for (var, value) in entries {
        payload.push(u64::from(*var), 4).boolean(*value);
    }
    Obj {
        create: false,
        router: 3,
        handle: Handle::server(id),
        ty: 0,
        seq,
        payload,
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
