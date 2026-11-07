// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! Replies and errors of the control channel (contract §3.2, §4.1).

use serde_json::{Map, Value, json};

/// `busy`'s `retryAfterMs`: the contract's default (§4.1, §4.6); never below 20.
pub const BUSY_RETRY_MS: u64 = 1000;

// error codes (contract §4.1)
pub const BAD_REQUEST: &str = "bad_request";
pub const UNKNOWN_OP: &str = "unknown_op";
pub const NOT_READY: &str = "not_ready";
pub const UNAUTHORIZED: &str = "unauthorized";
pub const PROTO_UNSUPPORTED: &str = "proto_unsupported";
pub const BUSY: &str = "busy";
pub const DRAINING: &str = "draining";
pub const CAPACITY: &str = "capacity";
pub const EXISTS: &str = "exists";
pub const NOT_FOUND: &str = "not_found";
pub const NO_KEYS: &str = "no_keys";
pub const CONTEXT_INVALID: &str = "context_invalid";
pub const STALE: &str = "stale";
pub const INTERNAL: &str = "internal";

/// An error reply's `error` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdError {
    pub code: &'static str,
    pub field: Option<String>,
    pub reason: Option<String>,
    pub retry_after_ms: Option<u64>,
}

impl CmdError {
    pub fn code(code: &'static str) -> CmdError {
        CmdError {
            code,
            field: None,
            reason: None,
            retry_after_ms: None,
        }
    }

    /// `bad_request` naming the field.
    pub fn bad(field: impl Into<String>) -> CmdError {
        CmdError {
            field: Some(field.into()),
            ..CmdError::code(BAD_REQUEST)
        }
    }

    pub fn busy(retry_after_ms: u64) -> CmdError {
        CmdError {
            retry_after_ms: Some(retry_after_ms.max(20)),
            ..CmdError::code(BUSY)
        }
    }

    pub fn reason(mut self, reason: impl Into<String>) -> CmdError {
        self.reason = Some(reason.into());
        self
    }

    pub fn to_value(&self) -> Value {
        let mut e = Map::new();
        e.insert("code".into(), Value::String(self.code.into()));
        if let Some(f) = &self.field {
            e.insert("field".into(), Value::String(f.clone()));
        }
        if let Some(r) = &self.reason {
            e.insert("reason".into(), Value::String(r.clone()));
        }
        if let Some(ms) = self.retry_after_ms {
            e.insert("retryAfterMs".into(), json!(ms));
        }
        Value::Object(e)
    }
}

pub type CmdResult = Result<Map<String, Value>, CmdError>;

/// `{"id":<id>,"ok":true, ...result}`
pub fn reply_ok(id: &Value, result: Map<String, Value>) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), id.clone());
    m.insert("ok".into(), Value::Bool(true));
    for (k, v) in result {
        if k != "id" && k != "ok" {
            m.insert(k, v);
        }
    }
    Value::Object(m)
}

/// `{"id":<id>,"ok":false,"error":{...}}`
pub fn reply_err(id: &Value, e: &CmdError) -> Value {
    json!({"id": id, "ok": false, "error": e.to_value()})
}

pub fn reply(id: &Value, r: CmdResult) -> Value {
    match r {
        Ok(m) => reply_ok(id, m),
        Err(e) => reply_err(id, &e),
    }
}

/// A framing error: `{"id":null,"ok":false,"error":{"code":"bad_request","field":<field>}}`.
pub fn framing_error(field: &str) -> Value {
    reply_err(&Value::Null, &CmdError::bad(field))
}

/// A line's JSON object; `Err("json")` when it is not UTF-8 JSON or not an object.
pub fn parse_line(bytes: &[u8]) -> Result<Value, &'static str> {
    let text = std::str::from_utf8(bytes).map_err(|_| "json")?;
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => Ok(v),
        _ => Err("json"),
    }
}

/// A request's `id`: a u32, else `None` (the reply then carries `null`).
pub fn request_id(request: &Value) -> Option<u32> {
    request
        .get("id")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}
