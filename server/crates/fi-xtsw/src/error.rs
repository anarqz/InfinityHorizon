//! Error types. Messages name types, field paths, offsets, lengths and kinds only, never decoded
//! contents (strings, numbers, blobs), so they are safe to log (services/AGENTS.md: no payloads in
//! logs).

use thiserror::Error;

/// A failure while decoding or encoding a `bin/xtsw` body or envelope.
///
/// `path` is the type the operation started from followed by the field path, for example
/// `ForzaUserLogon_Parameters.loginData.friendXuids[3]`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum XtswError {
    /// The class and method pair is not in the schema.
    #[error("unknown method {class}.{method}")]
    UnknownMethod {
        /// Service class without the assembly (`Forza.WebServices.User`), clipped to 128 bytes.
        class: String,
        /// Method name, clipped to 128 bytes.
        method: String,
    },
    /// The type name is not in the schema.
    #[error("unknown type {ty}")]
    UnknownType {
        /// The type name, clipped to 128 bytes.
        ty: String,
    },
    /// The type exists but is not a struct (the schema's primitive-named registrations).
    #[error("type {ty} is not a struct")]
    NotAStruct {
        /// The type name.
        ty: String,
    },
    /// A field of a kind the game's xtsw reader does not accept (0x08, 0x0b, 0x0f).
    #[error("{path}: kind {kind} is not accepted by the game's xtsw reader")]
    UnsupportedKind {
        /// Field path.
        path: String,
        /// The schema's kind name (`unknown_0xb`, ...).
        kind: String,
    },
    /// The input ends before the value does.
    #[error("{path}: needs {needed} bytes at offset {offset}, {available} left")]
    Truncated {
        /// Field path.
        path: String,
        /// Byte offset of the read.
        offset: usize,
        /// Bytes the value needs.
        needed: usize,
        /// Bytes left in the input.
        available: usize,
    },
    /// A length, count or character count above the game's limit or the field's capacity.
    #[error("{path}: {what} {len} above the limit {limit}")]
    LimitExceeded {
        /// Field path.
        path: String,
        /// What was measured: `string bytes`, `UTF-16 units`, `array count` or `blob bytes`.
        what: &'static str,
        /// The measured value.
        len: usize,
        /// The limit it crossed.
        limit: usize,
    },
    /// Structs and arrays nested deeper than [`crate::MAX_DEPTH`] (only a cyclic schema can).
    #[error("{path}: nested deeper than {limit} levels")]
    TooDeep {
        /// Field path.
        path: String,
        /// The depth limit.
        limit: usize,
    },
    /// Bytes left after the top-level value.
    #[error("{count} trailing bytes after {ty}")]
    TrailingBytes {
        /// The top-level type (or the envelope).
        ty: String,
        /// Number of bytes left over.
        count: usize,
    },
    /// A string or datetime that is not UTF-8 (the game's writer only produces UTF-8).
    #[error("{path}: invalid UTF-8 at offset {offset}")]
    InvalidUtf8 {
        /// Field path.
        path: String,
        /// Byte offset of the string's first byte.
        offset: usize,
    },
    /// A bool byte other than 0 or 1.
    #[error("{path}: bool byte above 1 at offset {offset}")]
    InvalidBool {
        /// Field path.
        path: String,
        /// Byte offset of the bool.
        offset: usize,
    },
    /// A value whose kind does not match the field's kind (encoding).
    #[error("{path}: expected {expected}, found {found}")]
    TypeMismatch {
        /// Field path.
        path: String,
        /// The kind the schema wants.
        expected: &'static str,
        /// The kind of the value given.
        found: &'static str,
    },
    /// A struct value without one of the type's fields (encoding a [`crate::Value`]; JSON input
    /// fills missing fields with defaults instead).
    #[error("{path}: missing field")]
    MissingField {
        /// Field path.
        path: String,
    },
    /// A JSON value that cannot become the field's kind (out of range, bad hex, bad guid), or a
    /// datetime outside the game's format where [`crate::Limits::game_datetimes`] asks for it.
    #[error("{path}: {reason}")]
    InvalidValue {
        /// Field path.
        path: String,
        /// Why, without the value itself.
        reason: &'static str,
    },
    /// A reply envelope whose status is neither 0 (success) nor 2 (error).
    #[error("envelope status {status} is neither 0 (ok) nor 2 (error)")]
    UnknownStatus {
        /// The status word.
        status: u32,
    },
}

/// A schema JSON that does not parse or does not hold together.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SchemaError {
    /// Not the schema JSON shape.
    #[error("schema JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A field or method names a type that is not defined.
    #[error("{owner}: references unknown type {target}")]
    UnknownTypeRef {
        /// `Type.field` or `Class.Method`.
        owner: String,
        /// The missing type name.
        target: String,
    },
    /// A struct field without its type, an array without its element, or an array of arrays.
    #[error("{owner}: {problem}")]
    Malformed {
        /// `Type.field`.
        owner: String,
        /// What is wrong.
        problem: &'static str,
    },
}

/// Keeps caller-provided names (from request headers) bounded in errors and logs.
pub(crate) fn clip(s: &str) -> String {
    const MAX: usize = 128;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}
