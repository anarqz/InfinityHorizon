//! The schema: services (class → method → `_Parameters` / `_Results` type) and type descriptors
//! (fields in wire order with their kinds and capacities), in the JSON shape written by the Beta's
//! `tools/extract_schema.py` from the game's own type registrations.

use std::collections::{BTreeMap, HashMap};

use fi_gamedata::Lazy;
use serde::Deserialize;

use crate::error::{SchemaError, XtswError, clip};
use crate::{Limits, Value, codec, json};

/// The game data file of FH2's schema (`FI_GAME_DATA_DIR`, extracted from the game's executable by
/// `tools/gamedata`; never in the crate).
pub const FH2_SCHEMA_FILE: &str = "fh2/xtsw-schema.json";

/// The game data file of Forza Motorsport 5's schema (OD-206).
pub const FM5_SCHEMA_FILE: &str = "fm5/xtsw-schema.json";

static FH2: Lazy<Schema> = Lazy::new(|| fi_gamedata::parse(FH2_SCHEMA_FILE, Schema::from_json));

static FM5: Lazy<Schema> = Lazy::new(|| fi_gamedata::parse(FM5_SCHEMA_FILE, Schema::from_json));

/// A wire kind of the game's generic xtsw reader (format notes, kinds table).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
pub enum Kind {
    /// 0x00: 1 byte.
    U8,
    /// 0x01: 2 bytes big-endian.
    I16,
    /// 0x02: 2 bytes big-endian.
    U16,
    /// 0x03: 4 bytes big-endian.
    I32,
    /// 0x04: 4 bytes big-endian.
    U32,
    /// 0x05: 8 bytes big-endian.
    I64,
    /// 0x06: 8 bytes big-endian.
    U64,
    /// 0x07: IEEE 754 single, 4 bytes big-endian.
    F32,
    /// 0x13: IEEE 754 double, 8 bytes big-endian.
    F64,
    /// 0x11: 1 byte, 0 or 1.
    Bool,
    /// 0x09: u32 big-endian byte length (at most 0x2000) then UTF-8.
    String,
    /// 0x10: a string `YYYY-MM-DD[THH:MM:SS[.mmm]][Z|+hh:mm|-hh:mm]`; the game rejects the whole
    /// reply when one does not parse.
    DateTime,
    /// 0x12: Data1 u32 BE, Data2 u16 BE, Data3 u16 BE, Data4 8 bytes (16 bytes in textual order).
    Guid,
    /// 0x14: u32 big-endian length (at most 0x80000 in replies) then the bytes.
    Blob,
    /// 0x0c: the type's fields in order, no tags.
    Struct,
    /// 0x0e: u32 big-endian count (at most 0x2000 and the capacity) then the elements.
    Array,
    /// Kinds the xtsw reader does not accept (0x08, 0x0b, 0x0f: Live, Thunderhead and Xbox Music
    /// types); decoding or encoding one fails with [`XtswError::UnsupportedKind`].
    Unsupported(String),
}

impl From<String> for Kind {
    fn from(name: String) -> Self {
        match name.as_str() {
            "u8" => Kind::U8,
            "i16" => Kind::I16,
            "u16" => Kind::U16,
            "i32" => Kind::I32,
            "u32" => Kind::U32,
            "i64" => Kind::I64,
            "u64" => Kind::U64,
            "f32" => Kind::F32,
            "f64" => Kind::F64,
            "bool" => Kind::Bool,
            "string" => Kind::String,
            "datetime" => Kind::DateTime,
            "guid" => Kind::Guid,
            "blob" => Kind::Blob,
            "struct" => Kind::Struct,
            "array" => Kind::Array,
            _ => Kind::Unsupported(name),
        }
    }
}

impl Kind {
    /// The schema's name of the kind (`u32`, `string`, `unknown_0xb`, ...).
    pub fn name(&self) -> &str {
        match self {
            Kind::Unsupported(name) => name,
            other => other.static_name(),
        }
    }

    pub(crate) fn static_name(&self) -> &'static str {
        match self {
            Kind::U8 => "u8",
            Kind::I16 => "i16",
            Kind::U16 => "u16",
            Kind::I32 => "i32",
            Kind::U32 => "u32",
            Kind::I64 => "i64",
            Kind::U64 => "u64",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Bool => "bool",
            Kind::String => "string",
            Kind::DateTime => "datetime",
            Kind::Guid => "guid",
            Kind::Blob => "blob",
            Kind::Struct => "struct",
            Kind::Array => "array",
            Kind::Unsupported(_) => "unsupported kind",
        }
    }
}

/// One service class (`Forza.WebServices.User`).
#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    /// The class as the game sends it in `X-ClassName` (`Forza.WebServices.User, Forza.WebServices`).
    #[serde(rename = "className")]
    pub class_name: String,
    /// Methods by name.
    pub methods: BTreeMap<String, Method>,
}

/// One method: the struct types of its request body and of its success reply.
#[derive(Debug, Clone, Deserialize)]
pub struct Method {
    /// The `_Parameters` type; `None` when the request body is empty.
    pub params: Option<String>,
    /// The `_Results` type; `None` when the success reply is the 12-byte header alone.
    pub results: Option<String>,
}

/// A registered type. Every type a method uses is a struct; the schema also holds eight
/// primitive-named registrations (`GUID`, `Int64`, ...) whose field lists are extraction
/// artifacts, which the codec refuses with [`XtswError::NotAStruct`].
#[derive(Debug, Clone, Deserialize)]
pub struct TypeDef {
    /// `Struct` for every usable type.
    pub kind: Kind,
    /// Fields in wire order.
    pub fields: Vec<Field>,
}

/// A struct field.
#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    /// Field name (the JSON key of [`Value::to_json`]).
    pub name: String,
    /// Wire kind.
    pub kind: Kind,
    /// The struct type of a `struct` field.
    #[serde(rename = "type")]
    pub ty: Option<String>,
    /// The element of an `array` field.
    pub elem: Option<Elem>,
    /// Capacity of a string in UTF-16 units (the game's in-memory WCHAR buffer, without the NUL).
    pub max_chars: Option<usize>,
    /// Capacity of an array (elements).
    pub max_count: Option<usize>,
    /// Capacity of a blob (bytes).
    pub max_len: Option<usize>,
}

/// The element of an array field.
#[derive(Debug, Clone, Deserialize)]
pub struct Elem {
    /// Element kind (never `array`).
    pub kind: Kind,
    /// The struct type of `struct` elements.
    #[serde(rename = "type")]
    pub ty: Option<String>,
}

/// A stub the extractor could not tie to a method (kept for reference).
#[derive(Debug, Clone, Deserialize)]
pub struct UnmatchedStub {
    /// Address of the stub in the executable.
    pub func: String,
    /// The class string the stub uses.
    pub class: String,
}

/// The xtsw schema: services and types. Build it with [`Schema::fh2`] (the game data's schema) or
/// [`Schema::from_json`].
#[derive(Debug, Deserialize)]
pub struct Schema {
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    kinds: BTreeMap<String, String>,
    services: BTreeMap<String, Service>,
    types: HashMap<String, TypeDef>,
    #[serde(default)]
    unresolved_registrations: Vec<String>,
    #[serde(default)]
    unmatched_stubs: Vec<UnmatchedStub>,
}

impl Schema {
    /// The game's schema ([`FH2_SCHEMA_FILE`] of the game data), parsed once. Panics with the game
    /// data's error when it cannot be loaded; a process checks it at start ([`Schema::try_fh2`]).
    pub fn fh2() -> &'static Schema {
        &FH2
    }

    /// Forza Motorsport 5's schema ([`FM5_SCHEMA_FILE`]), parsed once; panics like [`Schema::fh2`].
    pub fn fm5() -> &'static Schema {
        &FM5
    }

    /// [`Schema::fh2`], or why the game data has none.
    pub fn try_fh2() -> Result<&'static Schema, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// [`Schema::fm5`], or why the game data has none.
    pub fn try_fm5() -> Result<&'static Schema, &'static fi_gamedata::Error> {
        FM5.load()
    }

    /// The schema of `game`, `None` for a game without one.
    pub fn for_game(game: &str) -> Option<&'static Schema> {
        match game {
            "fh2" => Some(Self::fh2()),
            "fm5" => Some(Self::fm5()),
            _ => None,
        }
    }

    /// Parses a schema in the extractor's JSON shape and checks that every type reference
    /// resolves. Kinds the xtsw reader does not accept are kept as [`Kind::Unsupported`].
    pub fn from_json(json: &str) -> Result<Schema, SchemaError> {
        let schema: Schema = serde_json::from_str(json)?;
        schema.validate()?;
        Ok(schema)
    }

    fn validate(&self) -> Result<(), SchemaError> {
        let resolve = |owner: &dyn Fn() -> String, target: &str| {
            if self.types.contains_key(target) {
                Ok(())
            } else {
                Err(SchemaError::UnknownTypeRef {
                    owner: owner(),
                    target: target.to_owned(),
                })
            }
        };
        for (class, service) in &self.services {
            for (name, method) in &service.methods {
                for target in [&method.params, &method.results].into_iter().flatten() {
                    resolve(&|| format!("{class}.{name}"), target)?;
                }
            }
        }
        for (ty, def) in &self.types {
            for f in &def.fields {
                let owner = || format!("{ty}.{}", f.name);
                let malformed = |problem| SchemaError::Malformed {
                    owner: owner(),
                    problem,
                };
                match f.kind {
                    Kind::Struct => resolve(
                        &owner,
                        f.ty.as_deref()
                            .ok_or_else(|| malformed("struct without type"))?,
                    )?,
                    Kind::Array => {
                        let elem = f
                            .elem
                            .as_ref()
                            .ok_or_else(|| malformed("array without element"))?;
                        match elem.kind {
                            Kind::Array => return Err(malformed("array of arrays")),
                            Kind::Struct => resolve(
                                &owner,
                                elem.ty
                                    .as_deref()
                                    .ok_or_else(|| malformed("struct element without type"))?,
                            )?,
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// The method of a class. `class` may carry the assembly as the game sends it in
    /// `X-ClassName` (`Forza.WebServices.User, Forza.WebServices`).
    pub fn method(&self, class: &str, method: &str) -> Option<&Method> {
        self.services.get(class_key(class))?.methods.get(method)
    }

    /// Like [`Schema::method`], with [`XtswError::UnknownMethod`] when it is missing.
    pub fn require_method(&self, class: &str, method: &str) -> Result<&Method, XtswError> {
        self.method(class, method)
            .ok_or_else(|| XtswError::UnknownMethod {
                class: clip(class_key(class)),
                method: clip(method),
            })
    }

    /// A type by name.
    pub fn type_def(&self, name: &str) -> Option<&TypeDef> {
        self.types.get(name)
    }

    /// Services by class name (without the assembly), sorted.
    pub fn services(&self) -> impl Iterator<Item = (&str, &Service)> {
        self.services.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Every type by name, in no particular order.
    pub fn types(&self) -> impl Iterator<Item = (&str, &TypeDef)> {
        self.types.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// The extractor's kind table (`"0x9"` → `"string"`).
    pub fn kinds(&self) -> &BTreeMap<String, String> {
        &self.kinds
    }

    /// The executable the schema was extracted from.
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Type registrations the extractor could not name (addresses).
    pub fn unresolved_registrations(&self) -> &[String] {
        &self.unresolved_registrations
    }

    /// Stubs the extractor could not tie to a method.
    pub fn unmatched_stubs(&self) -> &[UnmatchedStub] {
        &self.unmatched_stubs
    }

    /// Decodes a struct of type `ty` that fills `bytes` exactly.
    pub fn decode(&self, ty: &str, bytes: &[u8], limits: &Limits) -> Result<Value, XtswError> {
        codec::decode(self, ty, bytes, limits)
    }

    /// Encodes a [`Value`] or a JSON value as a struct of type `ty` (see [`Encodable`]).
    pub fn encode<V: Encodable + ?Sized>(
        &self,
        ty: &str,
        value: &V,
        limits: &Limits,
    ) -> Result<Vec<u8>, XtswError> {
        let value = value.to_value(self, ty)?;
        codec::encode(self, ty, &value, limits)
    }

    /// Builds the [`Value`] of type `ty` from JSON, leniently, the way the Beta's handlers wrote
    /// replies: a missing or `null` field is zero, empty, false or [`crate::DEFAULT_DATETIME`];
    /// integers may be numbers, decimal or `0x` hex strings, or bools, in the signed or unsigned
    /// range of the width (two's complement); floats may be numbers or `"NaN"` / `"Infinity"` /
    /// `"-Infinity"`; guids are 32 hex digits (dashes and braces allowed); blobs are hex strings
    /// or byte arrays. Unknown keys are ignored. Capacities are checked when encoding.
    pub fn value_from_json(&self, ty: &str, json: &serde_json::Value) -> Result<Value, XtswError> {
        json::lower(self, ty, json)
    }
}

/// `Forza.WebServices.User, Forza.WebServices` → `Forza.WebServices.User`.
fn class_key(class: &str) -> &str {
    class.split(',').next().unwrap_or(class).trim()
}

/// Input of the encoders: a typed [`Value`] (encoded strictly: kinds must match and every field
/// must be present) or a [`serde_json::Value`] (converted with [`Schema::value_from_json`] first).
/// Sealed.
pub trait Encodable: sealed::Sealed {
    /// The typed value to encode as `ty`.
    fn to_value(&self, schema: &Schema, ty: &str)
    -> Result<std::borrow::Cow<'_, Value>, XtswError>;
}

impl Encodable for Value {
    fn to_value(&self, _: &Schema, _: &str) -> Result<std::borrow::Cow<'_, Value>, XtswError> {
        Ok(std::borrow::Cow::Borrowed(self))
    }
}

impl Encodable for serde_json::Value {
    fn to_value(
        &self,
        schema: &Schema,
        ty: &str,
    ) -> Result<std::borrow::Cow<'_, Value>, XtswError> {
        schema
            .value_from_json(ty, self)
            .map(std::borrow::Cow::Owned)
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for crate::Value {}
    impl Sealed for serde_json::Value {}
}
