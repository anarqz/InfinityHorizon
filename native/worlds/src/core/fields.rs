// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! Typed reads of a request's fields; every failure is `bad_request` naming the field's path.

use serde_json::{Map, Value};

use crate::control::proto::CmdError;

/// A JSON object and the path it sits at (`""` for the request, `"host"` for `host`, ...).
#[derive(Clone, Copy)]
pub struct Obj<'a> {
    pub map: &'a Map<String, Value>,
    pub path: &'a str,
}

pub fn path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

impl<'a> Obj<'a> {
    pub fn new(map: &'a Map<String, Value>, path: &'a str) -> Obj<'a> {
        Obj { map, path }
    }

    pub fn bad(&self, key: &str) -> CmdError {
        CmdError::bad(path(self.path, key))
    }

    pub fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.get(key).filter(|v| !v.is_null())
    }

    pub fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn str(&self, key: &str) -> Result<&'a str, CmdError> {
        self.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| self.bad(key))
    }

    pub fn bool(&self, key: &str) -> Result<bool, CmdError> {
        self.get(key)
            .and_then(Value::as_bool)
            .ok_or_else(|| self.bad(key))
    }

    pub fn opt_bool(&self, key: &str, default: bool) -> Result<bool, CmdError> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => v.as_bool().ok_or_else(|| self.bad(key)),
        }
    }

    /// An integer in `min..=max`.
    pub fn int(&self, key: &str, min: u64, max: u64) -> Result<u64, CmdError> {
        self.get(key)
            .and_then(Value::as_u64)
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| self.bad(key))
    }

    pub fn opt_int(&self, key: &str, min: u64, max: u64, default: u64) -> Result<u64, CmdError> {
        if self.has(key) {
            self.int(key, min, max)
        } else {
            Ok(default)
        }
    }

    pub fn obj(&self, key: &str) -> Result<&'a Map<String, Value>, CmdError> {
        self.get(key)
            .and_then(Value::as_object)
            .ok_or_else(|| self.bad(key))
    }

    pub fn arr(&self, key: &str) -> Result<&'a Vec<Value>, CmdError> {
        self.get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| self.bad(key))
    }

    pub fn xuid(&self, key: &str) -> Result<u64, CmdError> {
        self.get(key)
            .and_then(Value::as_str)
            .and_then(parse_xuid)
            .ok_or_else(|| self.bad(key))
    }

    pub fn uuid(&self, key: &str) -> Result<&'a str, CmdError> {
        let s = self.str(key)?;
        if canonical_uuid(s) {
            Ok(s)
        } else {
            Err(self.bad(key))
        }
    }
}

/// A XUID as on `/mm/v1`: a decimal string, digits only (no sign, spaces or separators; the C#
/// `NumberStyles.None`), fitting in a u64.
pub fn parse_xuid(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok()
}

/// A canonical lowercase `"D"`-format UUID, not all zeros.
pub fn canonical_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
        && b.iter().any(|c| *c != b'0' && *c != b'-')
}

/// A short text id (`world.close`'s `reason`): 1-64 printable ASCII characters, no spaces.
pub fn text_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| (0x21..=0x7e).contains(&b))
}
