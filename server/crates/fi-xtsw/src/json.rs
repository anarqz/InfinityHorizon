//! JSON → [`Value`] guided by the schema, with the Beta's lenient conventions for handler replies
//! (see [`crate::Schema::value_from_json`]).

use serde_json::Value as Json;

use crate::codec::{MAX_DEPTH, Seg, render_path, struct_def};
use crate::error::XtswError;
use crate::schema::{Elem, Kind, Schema};
use crate::value::Value;

/// The datetime written for a missing or `null` datetime: any valid date, because the game
/// rejects the whole reply when one datetime fails to parse.
pub const DEFAULT_DATETIME: &str = "2000-01-01T00:00:00Z";

const NULL: Json = Json::Null;

pub(crate) fn lower(schema: &Schema, ty: &str, json: &Json) -> Result<Value, XtswError> {
    let mut l = Lower {
        schema,
        root: ty,
        path: Vec::new(),
        depth: 0,
    };
    l.lower_struct(ty, json)
}

struct Lower<'a> {
    schema: &'a Schema,
    root: &'a str,
    path: Vec<Seg<'a>>,
    depth: usize,
}

fn json_kind(j: &Json) -> &'static str {
    match j {
        Json::Null => "null",
        Json::Bool(_) => "bool",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        Json::Array(_) => "array",
        Json::Object(_) => "object",
    }
}

impl<'a> Lower<'a> {
    fn path(&self) -> String {
        render_path(self.root, &self.path)
    }

    fn mismatch(&self, expected: &'static str, j: &Json) -> XtswError {
        XtswError::TypeMismatch {
            path: self.path(),
            expected,
            found: json_kind(j),
        }
    }

    fn invalid(&self, reason: &'static str) -> XtswError {
        XtswError::InvalidValue {
            path: self.path(),
            reason,
        }
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

    fn lower_struct(&mut self, ty: &'a str, j: &Json) -> Result<Value, XtswError> {
        let def = struct_def(self.schema, ty)?;
        let map = match j {
            Json::Object(map) => Some(map),
            Json::Null => None,
            other => return Err(self.mismatch("object", other)),
        };
        self.enter()?;
        let mut fields = Vec::with_capacity(def.fields.len());
        for f in &def.fields {
            self.path.push(Seg::Field(&f.name));
            let fj = map.and_then(|m| m.get(&f.name)).unwrap_or(&NULL);
            let value = self.lower_value(&f.kind, f.ty.as_deref(), f.elem.as_ref(), fj)?;
            self.path.pop();
            fields.push((f.name.clone(), value));
        }
        self.depth -= 1;
        Ok(Value::Struct(fields))
    }

    fn lower_value(
        &mut self,
        kind: &'a Kind,
        ty: Option<&'a str>,
        elem: Option<&'a Elem>,
        j: &Json,
    ) -> Result<Value, XtswError> {
        // the range checks in `int` make the truncating casts below two's complement conversions
        Ok(match kind {
            Kind::U8 => Value::U8(self.int(j, 8)? as u8),
            Kind::I16 => Value::I16(self.int(j, 16)? as i16),
            Kind::U16 => Value::U16(self.int(j, 16)? as u16),
            Kind::I32 => Value::I32(self.int(j, 32)? as i32),
            Kind::U32 => Value::U32(self.int(j, 32)? as u32),
            Kind::I64 => Value::I64(self.int(j, 64)? as i64),
            Kind::U64 => Value::U64(self.int(j, 64)? as u64),
            Kind::F32 => Value::F32(self.float(j)? as f32),
            Kind::F64 => Value::F64(self.float(j)?),
            Kind::Bool => Value::Bool(match j {
                Json::Bool(b) => *b,
                Json::Null | Json::Number(_) => self.int(j, 64)? != 0,
                other => return Err(self.mismatch("bool", other)),
            }),
            Kind::String => Value::String(match j {
                Json::Null => String::new(),
                Json::String(s) => s.clone(),
                other => return Err(self.mismatch("string", other)),
            }),
            Kind::DateTime => Value::DateTime(match j {
                Json::Null => DEFAULT_DATETIME.to_owned(),
                Json::String(s) => s.clone(),
                other => return Err(self.mismatch("datetime string", other)),
            }),
            Kind::Guid => Value::Guid(match j {
                Json::Null => [0; 16],
                Json::String(s) => {
                    let bytes = unhex(s.trim().trim_start_matches('{').trim_end_matches('}'))
                        .ok_or_else(|| self.invalid("a guid is 32 hex digits"))?;
                    bytes
                        .try_into()
                        .map_err(|_| self.invalid("a guid is 32 hex digits"))?
                }
                other => return Err(self.mismatch("guid string", other)),
            }),
            Kind::Blob => Value::Blob(match j {
                Json::Null => Vec::new(),
                Json::String(s) => {
                    unhex(s).ok_or_else(|| self.invalid("a blob is hex digits in pairs"))?
                }
                Json::Array(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        let byte = item.as_u64().and_then(|n| u8::try_from(n).ok());
                        out.push(
                            byte.ok_or_else(|| self.invalid("a blob array holds bytes 0 to 255"))?,
                        );
                    }
                    out
                }
                other => return Err(self.mismatch("blob hex string or byte array", other)),
            }),
            Kind::Struct => {
                return self.lower_struct(
                    ty.ok_or_else(|| XtswError::UnknownType { ty: String::new() })?,
                    j,
                );
            }
            Kind::Array => {
                let elem = elem.ok_or_else(|| XtswError::UnsupportedKind {
                    path: self.path(),
                    kind: "array".into(),
                })?;
                let items = match j {
                    Json::Null => return Ok(Value::Array(Vec::new())),
                    Json::Array(items) => items,
                    other => return Err(self.mismatch("array", other)),
                };
                self.enter()?;
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    self.path.push(Seg::Index(i));
                    out.push(self.lower_value(&elem.kind, elem.ty.as_deref(), None, item)?);
                    self.path.pop();
                }
                self.depth -= 1;
                Value::Array(out)
            }
            Kind::Unsupported(name) => {
                return Err(XtswError::UnsupportedKind {
                    path: self.path(),
                    kind: name.clone(),
                });
            }
        })
    }

    /// An integer in the signed or unsigned range of `bits` (so -1 is accepted for a u32 and
    /// becomes 0xffffffff, like the game's two's complement fields).
    fn int(&self, j: &Json, bits: u32) -> Result<i128, XtswError> {
        let n: i128 = match j {
            Json::Null => 0,
            Json::Bool(b) => i128::from(*b),
            Json::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
                (Some(i), _, _) => i.into(),
                (_, Some(u), _) => u.into(),
                (_, _, Some(f)) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1.9e19 => {
                    f as i128
                }
                _ => return Err(self.invalid("not an integer")),
            },
            Json::String(s) => {
                let s = s.trim();
                let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    Some(hex) => u64::from_str_radix(hex, 16).map(i128::from).ok(),
                    None => s.parse::<i128>().ok(),
                };
                parsed.ok_or_else(|| self.invalid("not an integer"))?
            }
            other => return Err(self.mismatch("integer", other)),
        };
        let min = -(1i128 << (bits - 1));
        let max = (1i128 << bits) - 1;
        if n < min || n > max {
            return Err(self.invalid("integer out of range for the field"));
        }
        Ok(n)
    }

    fn float(&self, j: &Json) -> Result<f64, XtswError> {
        match j {
            Json::Null => Ok(0.0),
            Json::Number(n) => n.as_f64().ok_or_else(|| self.invalid("not a number")),
            Json::String(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| self.invalid("not a number")),
            other => Err(self.mismatch("number", other)),
        }
    }
}

/// Hex digits in pairs; ASCII whitespace and dashes are ignored (guids).
fn unhex(s: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = s
        .bytes()
        .filter(|b| !b.is_ascii_whitespace() && *b != b'-')
        .collect();
    let (pairs, rest) = digits.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    let nibble = |b: u8| char::from(b).to_digit(16).map(|d| d as u8);
    pairs
        .iter()
        .map(|&[hi, lo]| Some(nibble(hi)? << 4 | nibble(lo)?))
        .collect()
}
