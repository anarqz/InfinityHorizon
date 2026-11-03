// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Synced-variable encodings (§5) and object payload variables (§4.5).
//!
//! An [`ObjectSchema`] is one object type's variable list (spec §7): it decodes and encodes a
//! payload's `(variable id, value)` pairs. [`object_schema`] returns the lists §7 gives so far.

use super::{
    CodecError,
    bits::{BitReader, BitRun, BitWriter, bit_length},
};

/// The wire format of one synced variable (§5). Values are raw: a template's range minimum is
/// not subtracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarType {
    /// `SyncedInt<unsigned T, …, bits>` or `SyncedEnum<E, default, bits>`: `bits` bits unsigned
    /// (`1..=32`).
    UInt { bits: u8 },
    /// `SyncedInt<signed T, …, bits>`: two's complement (§1.4), `2..=32` bits.
    SInt { bits: u8 },
    /// `SyncedInt<float, …, 32>`: raw IEEE-754 single.
    F32,
    /// `SyncedInt<64-bit T, …>`: 8 bytes, least significant first.
    U64,
    /// `SyncedBool`: 1 bit.
    Bool,
    /// `SyncedArray<T, Range, count, bits>`: `count` elements, all written every time, each in
    /// the scalar format `elem` (`UInt`, `SInt`, `F32`, `U64` or `Bool`).
    Array { elem: Box<VarType>, count: u16 },
    /// `SyncedString<wchar_t, max>`: length in `bits(max)` bits (rejected above `max`), then
    /// that many 16-bit UTF-16 code units.
    WString { max: u16 },
    /// `SyncedString<char, max>`: length in `bits(max)` bits (rejected above `max`), then that
    /// many 8-bit characters.
    CString { max: u16 },
}

/// A decoded variable value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarValue {
    UInt(u32),
    SInt(i32),
    /// The raw IEEE-754 bit pattern (exact round trips, NaN payloads included).
    F32(u32),
    U64(u64),
    Bool(bool),
    Array(Vec<VarValue>),
    WString(Vec<u16>),
    CString(Vec<u8>),
}

impl VarType {
    /// Checks the type's parameters (widths, scalar array elements, a non-zero string maximum).
    pub fn validate(&self) -> Result<(), CodecError> {
        match self {
            VarType::UInt { bits } if (1..=32).contains(bits) => Ok(()),
            VarType::SInt { bits } if (2..=32).contains(bits) => Ok(()),
            VarType::F32 | VarType::U64 | VarType::Bool => Ok(()),
            VarType::Array { elem, .. } => match **elem {
                VarType::Array { .. } | VarType::WString { .. } | VarType::CString { .. } => {
                    Err(CodecError::Layout)
                }
                ref e => e.validate(),
            },
            VarType::WString { max } | VarType::CString { max } if *max > 0 => Ok(()),
            _ => Err(CodecError::Layout),
        }
    }

    fn len_bits(max: u16) -> u8 {
        bit_length(u32::from(max))
    }

    /// Reads one value.
    pub fn read(&self, r: &mut BitReader<'_>) -> Result<VarValue, CodecError> {
        Ok(match self {
            VarType::UInt { bits } => VarValue::UInt(r.read(*bits)?),
            VarType::SInt { bits } => VarValue::SInt(r.read_signed(*bits)?),
            VarType::F32 => VarValue::F32(r.read_f32_bits()?),
            VarType::U64 => VarValue::U64(r.read_u64()?),
            VarType::Bool => VarValue::Bool(r.read_bool()?),
            VarType::Array { elem, count } => {
                self.validate()?;
                let mut v = Vec::with_capacity(usize::from(*count).min(r.remaining()));
                for _ in 0..*count {
                    v.push(elem.read(r)?);
                }
                VarValue::Array(v)
            }
            VarType::WString { max } => {
                let len = r.read(Self::len_bits(*max))? as u16;
                if len > *max {
                    return Err(CodecError::TooLong {
                        len: usize::from(len),
                        max: usize::from(*max),
                    });
                }
                let mut s = Vec::with_capacity(usize::from(len));
                for _ in 0..len {
                    s.push(r.read(16)? as u16);
                }
                VarValue::WString(s)
            }
            VarType::CString { max } => {
                let len = r.read(Self::len_bits(*max))? as u16;
                if len > *max {
                    return Err(CodecError::TooLong {
                        len: usize::from(len),
                        max: usize::from(*max),
                    });
                }
                let mut s = Vec::with_capacity(usize::from(len));
                for _ in 0..len {
                    s.push(r.read(8)? as u8);
                }
                VarValue::CString(s)
            }
        })
    }

    /// Writes one value; `Err(Layout)` when the value is not of this type.
    pub fn write(&self, v: &VarValue, w: &mut BitWriter) -> Result<(), CodecError> {
        match (self, v) {
            (VarType::UInt { bits }, VarValue::UInt(x)) => w.write(*x, *bits),
            (VarType::SInt { bits }, VarValue::SInt(x)) => w.write_signed(*x, *bits),
            (VarType::F32, VarValue::F32(x)) => w.write_f32_bits(*x),
            (VarType::U64, VarValue::U64(x)) => w.write_u64(*x),
            (VarType::Bool, VarValue::Bool(x)) => w.write_bool(*x),
            (VarType::Array { elem, count }, VarValue::Array(xs)) => {
                self.validate()?;
                if xs.len() != usize::from(*count) {
                    return Err(CodecError::Layout);
                }
                xs.iter().try_for_each(|x| elem.write(x, w))
            }
            (VarType::WString { max }, VarValue::WString(s)) => {
                if s.len() > usize::from(*max) {
                    return Err(CodecError::TooLong {
                        len: s.len(),
                        max: usize::from(*max),
                    });
                }
                w.write(s.len() as u32, Self::len_bits(*max))?;
                s.iter().try_for_each(|c| w.write(u32::from(*c), 16))
            }
            (VarType::CString { max }, VarValue::CString(s)) => {
                if s.len() > usize::from(*max) {
                    return Err(CodecError::TooLong {
                        len: s.len(),
                        max: usize::from(*max),
                    });
                }
                w.write(s.len() as u32, Self::len_bits(*max))?;
                s.iter().try_for_each(|c| w.write(u32::from(*c), 8))
            }
            _ => Err(CodecError::Layout),
        }
    }
}

/// One object type's variable list (§7): `vars[n]` is variable id `n`, in registration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSchema {
    pub name: &'static str,
    /// The constructor's capacity; the variable-id width is `bits(count)` (§4.5, §7). An object
    /// may register fewer variables than its count.
    pub count: u32,
    pub vars: Vec<(&'static str, VarType)>,
}

impl ObjectSchema {
    /// The variable-id width: the bit length of the variable count (§4.5).
    pub fn var_id_bits(&self) -> u8 {
        bit_length(self.count)
    }

    /// Decodes a payload's `(variable id, value)` pairs; they must fill exactly the payload.
    pub fn decode(&self, payload: &BitRun) -> Result<Vec<(u32, VarValue)>, CodecError> {
        let idb = self.var_id_bits();
        if idb == 0 {
            return if payload.is_empty() {
                Ok(Vec::new())
            } else {
                Err(CodecError::Payload)
            };
        }
        let mut r = payload.reader();
        let mut out = Vec::new();
        while r.remaining() > 0 {
            let id = r.read(idb).map_err(|_| CodecError::Payload)?;
            let (_, t) = self.vars.get(id as usize).ok_or(CodecError::Payload)?;
            let v = t.read(&mut r).map_err(|e| match e {
                CodecError::Truncated => CodecError::Payload,
                e => e,
            })?;
            out.push((id, v));
        }
        Ok(out)
    }

    /// Encodes `(variable id, value)` pairs into a payload (at most `2^14 - 1` bits).
    pub fn encode(&self, vars: &[(u32, VarValue)]) -> Result<BitRun, CodecError> {
        let idb = self.var_id_bits();
        let mut w = BitWriter::new();
        for (id, v) in vars {
            let (_, t) = self.vars.get(*id as usize).ok_or(CodecError::Layout)?;
            w.write(*id, idb)?;
            t.write(v, &mut w)?;
        }
        if w.len() > super::object::PAYLOAD_MAX_BITS {
            return Err(CodecError::Range);
        }
        Ok(w.into_run())
    }
}

/// `SessionHostSynced`, the Lobby router's (6) type 0 (§7.1): count 15, 13 registered.
pub fn session_host_synced() -> ObjectSchema {
    use VarType::{Bool, CString, F32, SInt, UInt};
    ObjectSchema {
        name: "SessionHostSynced",
        count: 15,
        vars: vec![
            ("LobbyType", UInt { bits: 3 }),
            ("PrivacyMode", UInt { bits: 2 }),
            ("CountdownType", UInt { bits: 2 }),
            ("CountdownEndTime", UInt { bits: 32 }),
            ("RaceReady", Bool),
            ("RaceParticipantsSelected", Bool),
            ("InRace", Bool),
            ("LastInactivePlayersPulledTime", UInt { bits: 32 }),
            ("RoundGuid", CString { max: 36 }),
            ("RaceStartTime", UInt { bits: 32 }),
            ("NumLapsCompleted", UInt { bits: 8 }),
            ("FurthestProgress", F32),
            ("GroupSatNavNode", SInt { bits: 32 }),
        ],
    }
}

/// `ConsoleSynced`, the ConsoleManager router's (3) only type (§7.2): count 9.
pub fn console_synced() -> ObjectSchema {
    use VarType::{Bool, UInt};
    ObjectSchema {
        name: "ConsoleSynced",
        count: 9,
        vars: vec![
            ("RenderReady", Bool),
            ("SessionEntryFinalised", Bool),
            ("ParticipatingInRace", Bool),
            ("RaceInProgress", Bool),
            ("Ready", Bool),
            ("HasLoadedCarsForAllParticipants", Bool),
            ("HasGrid", Bool),
            ("AvailableArbitration", UInt { bits: 2 }),
            ("MatchTicketArbiter", UInt { bits: 2 }),
        ],
    }
}

/// The variable list of `(router, type)` where §7 gives it; `None` until then (the payload stays
/// opaque: a receiver skips its `L` bits, §4.5).
pub fn object_schema(router: u8, type_index: u32) -> Option<ObjectSchema> {
    match (router, type_index) {
        // SPEC: §7 says "Lobby (6)" for SessionHostSynced and "the second type" for
        // GameOptionsSynced, so SessionHostSynced is taken as type 0.
        (6, 0) => Some(session_host_synced()),
        (3, 0) => Some(console_synced()),
        _ => None,
    }
}
