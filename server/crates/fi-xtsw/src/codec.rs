//! The positional big-endian reader and writer. Every length is checked against the remaining
//! input and the limits before anything is allocated; nesting is bounded by [`MAX_DEPTH`].

use std::fmt::Write as _;

use crate::error::{XtswError, clip};
use crate::schema::{Elem, Field, Kind, Schema, TypeDef};
use crate::value::Value;

/// Deepest struct and array nesting the codec follows. The game's schema nests at most 6 structs
/// and has no cycles; the limit protects against cyclic schemas given to [`Schema::from_json`].
pub const MAX_DEPTH: usize = 32;

/// Size limits of one direction of traffic. Field capacities from the schema (`max_chars`,
/// `max_count`, `max_len`) always apply on top of these, in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of one string or datetime.
    pub max_string_bytes: usize,
    /// Elements of one array.
    pub max_array_len: usize,
    /// Bytes of one blob.
    pub max_blob_len: usize,
    /// Whether every datetime must be one the game's reader parses ([`valid_datetime`]), in both
    /// directions so `encode(decode(bytes)) == bytes` still holds for what decodes.
    pub game_datetimes: bool,
}

impl Limits {
    /// What the game's reader accepts, so what every reply must respect: strings 0x2000 bytes,
    /// arrays 0x2000 elements, blobs 0x80000 bytes, datetimes in its format (one that does not
    /// parse fails the whole reply with `0x8050005c`).
    pub const RESULTS: Limits = Limits {
        max_string_bytes: 0x2000,
        max_array_len: 0x2000,
        max_blob_len: 0x80000,
        game_datetimes: true,
    };

    /// What the server accepts from the game's writer: the reader's limits, except blobs up to
    /// 32 MiB, because uploads without a capacity (ghosts, liveries, thumbnails, content) are not
    /// bound by the reader's 0x80000 (the Beta's `MAX_BLOB_IN`), and datetimes as the game wrote
    /// them (kept byte for byte, never refused).
    pub const PARAMS: Limits = Limits {
        max_blob_len: 32 << 20,
        game_datetimes: false,
        ..Limits::RESULTS
    };
}

/// Whether `text` is a datetime the game's xtsw reader parses into a `SYSTEMTIME`:
/// `YYYY-MM-DD[THH:MM:SS[.mmm]][Z|±hh:mm]` (Beta notes, `xtsw-format.md`), with a real calendar
/// date (years 0001 to 9999, leap years counted), hours 00-23, minutes and seconds 00-59, exactly
/// three fraction digits when there is a fraction, and an offset of at most 23:59.
pub fn valid_datetime(text: &str) -> bool {
    let b = text.as_bytes();
    let num = |at: usize, n: usize| -> Option<u32> {
        b.get(at..at + n)?.iter().try_fold(0u32, |acc, d| {
            d.is_ascii_digit().then(|| acc * 10 + u32::from(d - b'0'))
        })
    };
    let sep = |at: usize, c: u8| b.get(at) == Some(&c);
    let (Some(year), Some(month), Some(day)) = (num(0, 4), num(5, 2), num(8, 2)) else {
        return false;
    };
    if !sep(4, b'-') || !sep(7, b'-') || year == 0 || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > days {
        return false;
    }
    let mut at = 10;
    if sep(at, b'T') {
        let (Some(h), Some(m), Some(s)) = (num(11, 2), num(14, 2), num(17, 2)) else {
            return false;
        };
        if !sep(13, b':') || !sep(16, b':') || h > 23 || m > 59 || s > 59 {
            return false;
        }
        at = 19;
        if sep(at, b'.') {
            if num(20, 3).is_none() {
                return false;
            }
            at = 23;
        }
    }
    match b.get(at) {
        None => true,
        Some(b'Z') => at + 1 == b.len(),
        Some(b'+' | b'-') => {
            let (Some(h), Some(m)) = (num(at + 1, 2), num(at + 4, 2)) else {
                return false;
            };
            sep(at + 3, b':') && h <= 23 && m <= 59 && at + 6 == b.len()
        }
        Some(_) => false,
    }
}

/// The refusal of a datetime outside the game's format.
const DATETIME_FORMAT: &str = "a datetime the game parses: YYYY-MM-DD[THH:MM:SS[.mmm]][Z|±hh:mm]";

/// One step of a field path.
#[derive(Clone, Copy)]
pub(crate) enum Seg<'a> {
    Field(&'a str),
    Index(usize),
}

pub(crate) fn render_path(root: &str, segs: &[Seg<'_>]) -> String {
    let mut out = clip(root);
    for seg in segs {
        match seg {
            Seg::Field(name) => {
                out.push('.');
                out.push_str(name);
            }
            Seg::Index(i) => {
                let _ = write!(out, "[{i}]");
            }
        }
    }
    out
}

/// What a slot allows beyond its kind: the field's capacities and the element of arrays.
#[derive(Clone, Copy, Default)]
pub(crate) struct Caps<'a> {
    pub max_chars: Option<usize>,
    pub max_count: Option<usize>,
    pub max_len: Option<usize>,
    pub elem: Option<&'a Elem>,
}

impl<'a> Caps<'a> {
    pub(crate) fn of(f: &'a Field) -> Self {
        Caps {
            max_chars: f.max_chars,
            max_count: f.max_count,
            max_len: f.max_len,
            elem: f.elem.as_ref(),
        }
    }
}

fn cap(field: Option<usize>, global: usize) -> usize {
    field.map_or(global, |f| f.min(global))
}

/// The struct type definition, or the error the codec reports for it.
pub(crate) fn struct_def<'a>(schema: &'a Schema, ty: &str) -> Result<&'a TypeDef, XtswError> {
    let def = schema
        .type_def(ty)
        .ok_or_else(|| XtswError::UnknownType { ty: clip(ty) })?;
    if def.kind == Kind::Struct {
        Ok(def)
    } else {
        Err(XtswError::NotAStruct { ty: clip(ty) })
    }
}

pub(crate) fn decode(
    schema: &Schema,
    ty: &str,
    bytes: &[u8],
    limits: &Limits,
) -> Result<Value, XtswError> {
    let mut r = Reader {
        schema,
        limits,
        root: ty,
        bytes,
        pos: 0,
        path: Vec::new(),
        depth: 0,
    };
    let value = r.read_struct(ty)?;
    if r.pos != bytes.len() {
        return Err(XtswError::TrailingBytes {
            ty: clip(ty),
            count: bytes.len() - r.pos,
        });
    }
    Ok(value)
}

pub(crate) fn encode(
    schema: &Schema,
    ty: &str,
    value: &Value,
    limits: &Limits,
) -> Result<Vec<u8>, XtswError> {
    let mut w = Writer {
        schema,
        limits,
        root: ty,
        out: Vec::new(),
        path: Vec::new(),
        depth: 0,
    };
    w.write_struct(ty, value)?;
    Ok(w.out)
}

struct Reader<'a> {
    schema: &'a Schema,
    limits: &'a Limits,
    root: &'a str,
    bytes: &'a [u8],
    pos: usize,
    path: Vec<Seg<'a>>,
    depth: usize,
}

impl<'a> Reader<'a> {
    fn path(&self) -> String {
        render_path(self.root, &self.path)
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], XtswError> {
        let available = self.remaining();
        if n > available {
            return Err(XtswError::Truncated {
                path: self.path(),
                offset: self.pos,
                needed: n,
                available,
            });
        }
        let bytes = self.bytes;
        let slice = &bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], XtswError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// A u32 length or count, checked against `limit` before anything is read or allocated.
    fn len(&mut self, limit: usize, what: &'static str) -> Result<usize, XtswError> {
        let n = usize::try_from(u32::from_be_bytes(self.array()?)).unwrap_or(usize::MAX);
        if n > limit {
            return Err(XtswError::LimitExceeded {
                path: self.path(),
                what,
                len: n,
                limit,
            });
        }
        Ok(n)
    }

    fn enter(&mut self) -> Result<(), XtswError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(XtswError::TooDeep {
                path: self.path(),
                limit: MAX_DEPTH,
            });
        }
        Ok(())
    }

    fn read_struct(&mut self, ty: &'a str) -> Result<Value, XtswError> {
        let def = struct_def(self.schema, ty)?;
        self.enter()?;
        let mut fields = Vec::with_capacity(def.fields.len());
        for f in &def.fields {
            self.path.push(Seg::Field(&f.name));
            let value = self.read_value(&f.kind, f.ty.as_deref(), Caps::of(f))?;
            self.path.pop();
            fields.push((f.name.clone(), value));
        }
        self.depth -= 1;
        Ok(Value::Struct(fields))
    }

    fn read_value(
        &mut self,
        kind: &'a Kind,
        ty: Option<&'a str>,
        caps: Caps<'a>,
    ) -> Result<Value, XtswError> {
        Ok(match kind {
            Kind::U8 => Value::U8(self.array::<1>()?[0]),
            Kind::Bool => {
                let offset = self.pos;
                match self.array::<1>()?[0] {
                    0 => Value::Bool(false),
                    1 => Value::Bool(true),
                    _ => {
                        return Err(XtswError::InvalidBool {
                            path: self.path(),
                            offset,
                        });
                    }
                }
            }
            Kind::I16 => Value::I16(i16::from_be_bytes(self.array()?)),
            Kind::U16 => Value::U16(u16::from_be_bytes(self.array()?)),
            Kind::I32 => Value::I32(i32::from_be_bytes(self.array()?)),
            Kind::U32 => Value::U32(u32::from_be_bytes(self.array()?)),
            Kind::I64 => Value::I64(i64::from_be_bytes(self.array()?)),
            Kind::U64 => Value::U64(u64::from_be_bytes(self.array()?)),
            Kind::F32 => Value::F32(f32::from_bits(u32::from_be_bytes(self.array()?))),
            Kind::F64 => Value::F64(f64::from_bits(u64::from_be_bytes(self.array()?))),
            Kind::Guid => Value::Guid(self.array()?),
            Kind::String | Kind::DateTime => {
                let n = self.len(self.limits.max_string_bytes, "string bytes")?;
                let offset = self.pos;
                let raw = self.take(n)?;
                let text = std::str::from_utf8(raw).map_err(|_| XtswError::InvalidUtf8 {
                    path: self.path(),
                    offset,
                })?;
                if let Some(max) = caps.max_chars {
                    let units = text.encode_utf16().count();
                    if units > max {
                        return Err(XtswError::LimitExceeded {
                            path: self.path(),
                            what: "UTF-16 units",
                            len: units,
                            limit: max,
                        });
                    }
                }
                if *kind == Kind::String {
                    Value::String(text.to_owned())
                } else {
                    if self.limits.game_datetimes && !valid_datetime(text) {
                        return Err(XtswError::InvalidValue {
                            path: self.path(),
                            reason: DATETIME_FORMAT,
                        });
                    }
                    Value::DateTime(text.to_owned())
                }
            }
            Kind::Blob => {
                let n = self.len(cap(caps.max_len, self.limits.max_blob_len), "blob bytes")?;
                Value::Blob(self.take(n)?.to_vec())
            }
            Kind::Struct => {
                self.read_struct(ty.ok_or_else(|| XtswError::UnknownType { ty: String::new() })?)?
            }
            Kind::Array => {
                let elem = caps.elem.ok_or_else(|| XtswError::UnsupportedKind {
                    path: self.path(),
                    kind: "array".into(),
                })?;
                let n = self.len(
                    cap(caps.max_count, self.limits.max_array_len),
                    "array count",
                )?;
                self.enter()?;
                // every element takes at least one byte: never reserve more than the input holds
                let mut items = Vec::with_capacity(n.min(self.remaining()));
                for i in 0..n {
                    self.path.push(Seg::Index(i));
                    items.push(self.read_value(&elem.kind, elem.ty.as_deref(), Caps::default())?);
                    self.path.pop();
                }
                self.depth -= 1;
                Value::Array(items)
            }
            Kind::Unsupported(name) => {
                return Err(XtswError::UnsupportedKind {
                    path: self.path(),
                    kind: name.clone(),
                });
            }
        })
    }
}

struct Writer<'a> {
    schema: &'a Schema,
    limits: &'a Limits,
    root: &'a str,
    out: Vec<u8>,
    path: Vec<Seg<'a>>,
    depth: usize,
}

impl<'a> Writer<'a> {
    fn path(&self) -> String {
        render_path(self.root, &self.path)
    }

    fn enter(&mut self) -> Result<(), XtswError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(XtswError::TooDeep {
                path: self.path(),
                limit: MAX_DEPTH,
            });
        }
        Ok(())
    }

    fn put_len(&mut self, len: usize, limit: usize, what: &'static str) -> Result<(), XtswError> {
        let limit = limit.min(u32::MAX as usize);
        if len > limit {
            return Err(XtswError::LimitExceeded {
                path: self.path(),
                what,
                len,
                limit,
            });
        }
        // len <= limit <= u32::MAX
        self.out.extend_from_slice(&(len as u32).to_be_bytes());
        Ok(())
    }

    fn put_text(&mut self, text: &str, caps: Caps<'_>) -> Result<(), XtswError> {
        if let Some(max) = caps.max_chars {
            let units = text.encode_utf16().count();
            if units > max {
                return Err(XtswError::LimitExceeded {
                    path: self.path(),
                    what: "UTF-16 units",
                    len: units,
                    limit: max,
                });
            }
        }
        self.put_len(text.len(), self.limits.max_string_bytes, "string bytes")?;
        self.out.extend_from_slice(text.as_bytes());
        Ok(())
    }

    fn write_struct(&mut self, ty: &'a str, value: &Value) -> Result<(), XtswError> {
        let def = struct_def(self.schema, ty)?;
        let Value::Struct(fields) = value else {
            return Err(XtswError::TypeMismatch {
                path: self.path(),
                expected: "struct",
                found: value.kind_name(),
            });
        };
        self.enter()?;
        for (i, f) in def.fields.iter().enumerate() {
            self.path.push(Seg::Field(&f.name));
            let field_value = match fields.get(i) {
                Some((name, v)) if *name == f.name => v,
                _ => match fields.iter().find(|(name, _)| *name == f.name) {
                    Some((_, v)) => v,
                    None => return Err(XtswError::MissingField { path: self.path() }),
                },
            };
            self.write_value(&f.kind, f.ty.as_deref(), Caps::of(f), field_value)?;
            self.path.pop();
        }
        self.depth -= 1;
        Ok(())
    }

    fn write_value(
        &mut self,
        kind: &'a Kind,
        ty: Option<&'a str>,
        caps: Caps<'a>,
        value: &Value,
    ) -> Result<(), XtswError> {
        match (kind, value) {
            (Kind::U8, Value::U8(v)) => self.out.push(*v),
            (Kind::Bool, Value::Bool(v)) => self.out.push(u8::from(*v)),
            (Kind::I16, Value::I16(v)) => self.out.extend_from_slice(&v.to_be_bytes()),
            (Kind::U16, Value::U16(v)) => self.out.extend_from_slice(&v.to_be_bytes()),
            (Kind::I32, Value::I32(v)) => self.out.extend_from_slice(&v.to_be_bytes()),
            (Kind::U32, Value::U32(v)) => self.out.extend_from_slice(&v.to_be_bytes()),
            (Kind::I64, Value::I64(v)) => self.out.extend_from_slice(&v.to_be_bytes()),
            (Kind::U64, Value::U64(v)) => self.out.extend_from_slice(&v.to_be_bytes()),
            (Kind::F32, Value::F32(v)) => self.out.extend_from_slice(&v.to_bits().to_be_bytes()),
            (Kind::F64, Value::F64(v)) => self.out.extend_from_slice(&v.to_bits().to_be_bytes()),
            (Kind::Guid, Value::Guid(v)) => self.out.extend_from_slice(v),
            (Kind::String, Value::String(text)) => self.put_text(text, caps)?,
            (Kind::DateTime, Value::DateTime(text)) => {
                if self.limits.game_datetimes && !valid_datetime(text) {
                    return Err(XtswError::InvalidValue {
                        path: self.path(),
                        reason: DATETIME_FORMAT,
                    });
                }
                self.put_text(text, caps)?
            }
            (Kind::Blob, Value::Blob(bytes)) => {
                self.put_len(
                    bytes.len(),
                    cap(caps.max_len, self.limits.max_blob_len),
                    "blob bytes",
                )?;
                self.out.extend_from_slice(bytes);
            }
            (Kind::Struct, _) => self.write_struct(
                ty.ok_or_else(|| XtswError::UnknownType { ty: String::new() })?,
                value,
            )?,
            (Kind::Array, Value::Array(items)) => {
                let elem = caps.elem.ok_or_else(|| XtswError::UnsupportedKind {
                    path: self.path(),
                    kind: "array".into(),
                })?;
                // the game stops parsing at a count above the capacity and leaves the rest unread
                self.put_len(
                    items.len(),
                    cap(caps.max_count, self.limits.max_array_len),
                    "array count",
                )?;
                self.enter()?;
                for (i, item) in items.iter().enumerate() {
                    self.path.push(Seg::Index(i));
                    self.write_value(&elem.kind, elem.ty.as_deref(), Caps::default(), item)?;
                    self.path.pop();
                }
                self.depth -= 1;
            }
            (Kind::Unsupported(name), _) => {
                return Err(XtswError::UnsupportedKind {
                    path: self.path(),
                    kind: name.clone(),
                });
            }
            (kind, value) => {
                return Err(XtswError::TypeMismatch {
                    path: self.path(),
                    expected: kind.static_name(),
                    found: value.kind_name(),
                });
            }
        }
        Ok(())
    }
}
