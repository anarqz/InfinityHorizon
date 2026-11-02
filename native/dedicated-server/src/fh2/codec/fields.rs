// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written from docs/launcher/NERDCORE_WIRE.md (OD-187); no other codec source used.
//! The field notation of the message catalogue (§6.2): a message body is a list of [`Field`]s,
//! decoded into the same number of [`FieldValue`]s.

use super::{
    CodecError,
    bits::{BitReader, BitWriter},
    object::ObjectKey,
};

/// One token of §6.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// `uN`: unsigned, `1..=32` bits.
    U(u8),
    /// `sN`: two's complement, `2..=32` bits.
    S(u8),
    /// `bool`: 1 bit.
    Bool,
    /// `f32`: raw IEEE-754 single.
    F32,
    /// `vec3f`: three `f32` (x, y, z).
    Vec3,
    /// `u64`: 8 bytes, least significant first.
    U64,
    /// `blob64`: a byte run whose count is always 8, then 8 bytes.
    Blob64,
    /// `str(N)`: a string of at most N bytes.
    Str(u8),
    /// `key(N)`: an object key with an N-bit id (§1.7).
    Key(u8),
    /// `uC {loop …}`: a C-bit count, then that many repetitions of `item`.
    Repeat {
        count_bits: u8,
        item: &'static [Field],
    },
}

/// A decoded field. Floats are kept as their raw bit patterns so that bodies round-trip exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    U(u32),
    S(i32),
    Bool(bool),
    F32(u32),
    Vec3([u32; 3]),
    U64(u64),
    Blob64([u8; 8]),
    Str(Vec<u8>),
    Key(ObjectKey),
    Repeat(Vec<Vec<FieldValue>>),
}

/// The byte count of a `blob64` (§6.2: "count field (always 8)").
pub const BLOB64_LEN: usize = 8;

impl Field {
    pub fn read(&self, r: &mut BitReader<'_>) -> Result<FieldValue, CodecError> {
        Ok(match *self {
            Field::U(n) => FieldValue::U(r.read(n)?),
            Field::S(n) => FieldValue::S(r.read_signed(n)?),
            Field::Bool => FieldValue::Bool(r.read_bool()?),
            Field::F32 => FieldValue::F32(r.read_f32_bits()?),
            Field::Vec3 => {
                FieldValue::Vec3([r.read_f32_bits()?, r.read_f32_bits()?, r.read_f32_bits()?])
            }
            Field::U64 => FieldValue::U64(r.read_u64()?),
            Field::Blob64 => {
                // SPEC: unverified (§6.2): only the count 8 is accepted; the spec says the count
                // is always 8 and does not say how the game's reader treats a shorter run.
                let b = r.read_byte_run(BLOB64_LEN)?;
                let arr: [u8; 8] = b.try_into().map_err(|_| CodecError::Range)?;
                FieldValue::Blob64(arr)
            }
            Field::Str(max) => FieldValue::Str(r.read_string(usize::from(max))?),
            Field::Key(n) => FieldValue::Key(ObjectKey::read(r, n)?),
            Field::Repeat { count_bits, item } => {
                let count = r.read(count_bits)? as usize;
                let mut out = Vec::with_capacity(count.min(r.remaining()));
                for _ in 0..count {
                    out.push(read_fields(item, r)?);
                }
                FieldValue::Repeat(out)
            }
        })
    }

    pub fn write(&self, v: &FieldValue, w: &mut BitWriter) -> Result<(), CodecError> {
        match (*self, v) {
            (Field::U(n), FieldValue::U(x)) => w.write(*x, n),
            (Field::S(n), FieldValue::S(x)) => w.write_signed(*x, n),
            (Field::Bool, FieldValue::Bool(x)) => w.write_bool(*x),
            (Field::F32, FieldValue::F32(x)) => w.write_f32_bits(*x),
            (Field::Vec3, FieldValue::Vec3(xs)) => xs.iter().try_for_each(|x| w.write_f32_bits(*x)),
            (Field::U64, FieldValue::U64(x)) => w.write_u64(*x),
            (Field::Blob64, FieldValue::Blob64(b)) => w.write_byte_run(b, BLOB64_LEN),
            (Field::Str(max), FieldValue::Str(s)) => w.write_string(s, usize::from(max)),
            (Field::Key(n), FieldValue::Key(k)) => k.write(w, n),
            (Field::Repeat { count_bits, item }, FieldValue::Repeat(rows)) => {
                let count = u32::try_from(rows.len()).map_err(|_| CodecError::Range)?;
                w.write(count, count_bits)?;
                rows.iter().try_for_each(|row| write_fields(item, row, w))
            }
            _ => Err(CodecError::Layout),
        }
    }
}

/// Reads every field of a layout in order.
pub fn read_fields(layout: &[Field], r: &mut BitReader<'_>) -> Result<Vec<FieldValue>, CodecError> {
    layout.iter().map(|f| f.read(r)).collect()
}

/// Writes values against a layout; the number and kinds of values must match.
pub fn write_fields(
    layout: &[Field],
    values: &[FieldValue],
    w: &mut BitWriter,
) -> Result<(), CodecError> {
    if layout.len() != values.len() {
        return Err(CodecError::Layout);
    }
    layout
        .iter()
        .zip(values)
        .try_for_each(|(f, v)| f.write(v, w))
}
