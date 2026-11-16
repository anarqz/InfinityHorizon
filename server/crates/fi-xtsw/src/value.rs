//! The dynamic value model: one variant per wire kind, so a decoded body re-encodes byte for byte
//! (integer widths, f32 against f64 and NaN bit patterns included), and a lossless-enough mapping
//! to JSON for handlers.

use serde_json::{Map, Number, Value as Json};

/// A decoded xtsw value. Structs keep the schema's field order.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `u8`.
    U8(u8),
    /// `i16`.
    I16(i16),
    /// `u16`.
    U16(u16),
    /// `i32`.
    I32(i32),
    /// `u32`.
    U32(u32),
    /// `i64`.
    I64(i64),
    /// `u64`.
    U64(u64),
    /// `f32`, bit pattern preserved.
    F32(f32),
    /// `f64`, bit pattern preserved.
    F64(f64),
    /// `bool`.
    Bool(bool),
    /// `string`.
    String(String),
    /// `datetime`: the string as sent (`YYYY-MM-DD[THH:MM:SS[.mmm]][Z|+hh:mm|-hh:mm]`).
    DateTime(String),
    /// `guid`: the 16 wire bytes, which are in textual order (Data1, Data2, Data3 big-endian).
    Guid([u8; 16]),
    /// `blob`.
    Blob(Vec<u8>),
    /// `struct`: `(field name, value)` in wire order.
    Struct(Vec<(String, Value)>),
    /// `array`.
    Array(Vec<Value>),
}

impl Value {
    /// The kind name (`u32`, `string`, `struct`, ...), as in the schema.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::U8(_) => "u8",
            Value::I16(_) => "i16",
            Value::U16(_) => "u16",
            Value::I32(_) => "i32",
            Value::U32(_) => "u32",
            Value::I64(_) => "i64",
            Value::U64(_) => "u64",
            Value::F32(_) => "f32",
            Value::F64(_) => "f64",
            Value::Bool(_) => "bool",
            Value::String(_) => "string",
            Value::DateTime(_) => "datetime",
            Value::Guid(_) => "guid",
            Value::Blob(_) => "blob",
            Value::Struct(_) => "struct",
            Value::Array(_) => "array",
        }
    }

    /// A struct field by name.
    pub fn get(&self, field: &str) -> Option<&Value> {
        match self {
            Value::Struct(fields) => fields
                .iter()
                .find(|(name, _)| name == field)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Any integer kind whose value fits in `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::U8(v) => Some(v.into()),
            Value::I16(v) => Some(v.into()),
            Value::U16(v) => Some(v.into()),
            Value::I32(v) => Some(v.into()),
            Value::U32(v) => Some(v.into()),
            Value::I64(v) => Some(v),
            Value::U64(v) => i64::try_from(v).ok(),
            _ => None,
        }
    }

    /// Any integer kind whose value fits in `u64` (not negative).
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Value::U64(v) => Some(v),
            _ => self.as_i64().and_then(|v| u64::try_from(v).ok()),
        }
    }

    /// `f32` (widened exactly) or `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::F32(v) => Some(v.into()),
            Value::F64(v) => Some(v),
            _ => None,
        }
    }

    /// `bool`.
    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Value::Bool(v) => Some(v),
            _ => None,
        }
    }

    /// `string` or `datetime`.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) | Value::DateTime(s) => Some(s),
            _ => None,
        }
    }

    /// `blob`.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }

    /// `array`.
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// JSON for handlers and logs of shapes: integers and finite floats as numbers (`f32` widened
    /// exactly to `f64`), non-finite floats as `"NaN"` / `"Infinity"` / `"-Infinity"`, strings and
    /// datetimes as strings, guids as `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, blobs as lowercase
    /// hex, structs as objects. [`crate::Schema::value_from_json`] maps it back to the same bytes,
    /// except NaN payloads.
    pub fn to_json(&self) -> Json {
        match self {
            Value::U8(v) => (*v).into(),
            Value::I16(v) => (*v).into(),
            Value::U16(v) => (*v).into(),
            Value::I32(v) => (*v).into(),
            Value::U32(v) => (*v).into(),
            Value::I64(v) => (*v).into(),
            Value::U64(v) => (*v).into(),
            Value::F32(v) => float_json(f64::from(*v)),
            Value::F64(v) => float_json(*v),
            Value::Bool(v) => (*v).into(),
            Value::String(s) | Value::DateTime(s) => Json::String(s.clone()),
            Value::Guid(bytes) => Json::String(guid_string(bytes)),
            Value::Blob(bytes) => Json::String(hex(bytes)),
            Value::Struct(fields) => Json::Object(
                fields
                    .iter()
                    .map(|(name, v)| (name.clone(), v.to_json()))
                    .collect::<Map<_, _>>(),
            ),
            Value::Array(items) => Json::Array(items.iter().map(Value::to_json).collect()),
        }
    }

    /// [`Value::to_json`] consuming the value: names and strings are moved and each node is freed
    /// as it is converted, so a large request is not held twice (typed and JSON) at once.
    pub fn into_json(self) -> Json {
        match self {
            Value::String(s) | Value::DateTime(s) => Json::String(s),
            Value::Struct(fields) => Json::Object(
                fields
                    .into_iter()
                    .map(|(name, v)| (name, v.into_json()))
                    .collect::<Map<_, _>>(),
            ),
            Value::Array(items) => Json::Array(items.into_iter().map(Value::into_json).collect()),
            scalar => scalar.to_json(),
        }
    }
}

fn float_json(v: f64) -> Json {
    match Number::from_f64(v) {
        Some(n) => Json::Number(n),
        None if v.is_nan() => Json::String("NaN".into()),
        None if v > 0.0 => Json::String("Infinity".into()),
        None => Json::String("-Infinity".into()),
    }
}

/// Lowercase hex of bytes.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` from the 16 wire bytes.
pub fn guid_string(bytes: &[u8; 16]) -> String {
    let h = hex(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// The longest prefix of `text` that fits in `max` UTF-16 units, cut at a character boundary.
/// Encoding refuses a string above its field's `max_chars`; handlers that hold longer text (names
/// from the database) cut it with this first, as the game would.
pub fn truncate_utf16(text: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &text[..i];
        }
    }
    text
}
