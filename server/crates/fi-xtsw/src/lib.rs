//! `bin/xtsw` codec of Forza Horizon 2's Xls web services (SER-3.1).
//!
//! The game calls its web services with `POST .../o.xtsw`, `Content-Type: bin/xtsw`, the class in
//! `X-ClassName` (`Forza.WebServices.User, Forza.WebServices`) and the method in `X-Method`. The
//! body is the method's `_Parameters` struct; the reply is a 12-byte envelope ([`ok_envelope`],
//! [`error_envelope`], [`parse_envelope`]) followed, on success, by the `_Results` struct.
//!
//! The format, as read from the game's own reader (Beta notes `local-docs/xtsw-format.md`):
//! big-endian and positional, fields in the type's order with no tags; `string` and `datetime`
//! are a u32 byte length (at most 0x2000) and UTF-8; `blob` a u32 length (at most 0x80000 in
//! replies) and bytes; `array` a u32 count (at most 0x2000 and the field's capacity) and the
//! elements; `guid` 16 bytes (Data1 u32, Data2 u16, Data3 u16 big-endian, Data4); `bool` and `u8`
//! one byte; `f32`/`f64` IEEE big-endian. Kinds 0x08, 0x0b and 0x0f are not accepted by the
//! reader ([`Kind::Unsupported`]).
//!
//! The schema ([`Schema::fh2`]) is the game data's `fh2/xtsw-schema.json` (`FI_GAME_DATA_DIR`,
//! crate `fi-gamedata`), extracted from the operator's own game executable by `tools/gamedata`;
//! nothing of it is compiled in.
//!
//! Guarantees: `encode(decode(bytes)) == bytes` for every body [`decode_params`] or
//! [`decode_results`] accepts; hostile input never panics and never allocates more than the input
//! and the limits allow ([`Limits`], [`MAX_DEPTH`]); errors never carry decoded contents. Replies
//! ([`encode_results`], [`decode_results`]) carry only datetimes the game parses
//! ([`valid_datetime`]); requests keep the game's datetimes as written.

mod codec;
mod envelope;
mod error;
mod json;
mod schema;
mod value;

pub use codec::{Limits, MAX_DEPTH, valid_datetime};
pub use envelope::{
    Envelope, HEADER_LEN, MAX_ERROR_MESSAGE, STATUS_ERROR, STATUS_OK, error_envelope, ok_envelope,
    parse_envelope,
};
pub use error::{SchemaError, XtswError};
pub use json::DEFAULT_DATETIME;
pub use schema::{
    Elem, Encodable, FH2_SCHEMA_FILE, FM5_SCHEMA_FILE, Field, Kind, Method, Schema, Service,
    TypeDef, UnmatchedStub,
};
pub use value::{Value, guid_string, hex, truncate_utf16};

/// Decodes a request body (the method's `_Parameters`) with [`Limits::PARAMS`]. A method without
/// a `_Parameters` type takes an empty body and decodes to an empty struct.
pub fn decode_params(
    schema: &Schema,
    class: &str,
    method: &str,
    body: &[u8],
) -> Result<Value, XtswError> {
    let m = schema.require_method(class, method)?;
    decode_body(
        schema,
        m.params.as_deref(),
        body,
        &Limits::PARAMS,
        "(no _Parameters)",
    )
}

/// Encodes a request body (the method's `_Parameters`) with [`Limits::PARAMS`]: for tests,
/// simulators and golden round-trips. A method without a `_Parameters` type has an empty body.
pub fn encode_params<V: Encodable + ?Sized>(
    schema: &Schema,
    class: &str,
    method: &str,
    value: &V,
) -> Result<Vec<u8>, XtswError> {
    let m = schema.require_method(class, method)?;
    encode_body(schema, m.params.as_deref(), value, &Limits::PARAMS)
}

/// Decodes the `_Results` of a success reply (the bytes after the 12-byte header, see
/// [`parse_envelope`]) with [`Limits::RESULTS`]. A method without a `_Results` type has none.
pub fn decode_results(
    schema: &Schema,
    class: &str,
    method: &str,
    results: &[u8],
) -> Result<Value, XtswError> {
    let m = schema.require_method(class, method)?;
    decode_body(
        schema,
        m.results.as_deref(),
        results,
        &Limits::RESULTS,
        "(no _Results)",
    )
}

/// Encodes the `_Results` of a success reply with [`Limits::RESULTS`] (wrap them with
/// [`ok_envelope`]). A method without a `_Results` type encodes to nothing, whatever `value` is.
pub fn encode_results<V: Encodable + ?Sized>(
    schema: &Schema,
    class: &str,
    method: &str,
    value: &V,
) -> Result<Vec<u8>, XtswError> {
    let m = schema.require_method(class, method)?;
    encode_body(schema, m.results.as_deref(), value, &Limits::RESULTS)
}

fn decode_body(
    schema: &Schema,
    ty: Option<&str>,
    bytes: &[u8],
    limits: &Limits,
    none: &str,
) -> Result<Value, XtswError> {
    match ty {
        Some(ty) => schema.decode(ty, bytes, limits),
        None if bytes.is_empty() => Ok(Value::Struct(Vec::new())),
        None => Err(XtswError::TrailingBytes {
            ty: none.into(),
            count: bytes.len(),
        }),
    }
}

fn encode_body<V: Encodable + ?Sized>(
    schema: &Schema,
    ty: Option<&str>,
    value: &V,
    limits: &Limits,
) -> Result<Vec<u8>, XtswError> {
    match ty {
        Some(ty) => schema.encode(ty, value, limits),
        None => Ok(Vec::new()),
    }
}
