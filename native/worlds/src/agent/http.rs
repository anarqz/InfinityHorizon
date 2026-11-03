// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The signed calls to the server's official routes (MATCHMAKING_API §2.9): one blocking HTTP/1.1
//! client (ureq + rustls), every request carrying `FI-World-Agent` (WORLDS_OFFICIAL.md §4.1).
//! Answers are status, `Retry-After` and body; nothing here logs (the caller logs the route name
//! and the status, never a URL, a body or a header).

use std::time::Duration;

use ed25519_dalek::SigningKey;

use super::sign;

/// The most bytes read from an answer (a `WorldAnnounced` with 256 punches stays far below).
const BODY_MAX: u64 = 1024 * 1024;

pub struct Reply {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

/// Shared by the loop and the game-session calls (ureq's agent is `Send + Sync`).
pub struct Client {
    agent: ureq::Agent,
    base: String,
    vm_id: String,
    key: SigningKey,
}

impl Client {
    /// The loop's client: every call ends within 10 s.
    pub fn new(base: &str, vm_id: &str, key: SigningKey) -> Client {
        Client::with_timeout(base, vm_id, key, Duration::from_secs(10))
    }

    /// A client whose calls end within `timeout` (the game-session calls: past their 14 s
    /// budget, so the agent's own deadline answers first).
    pub fn with_timeout(base: &str, vm_id: &str, key: SigningKey, timeout: Duration) -> Client {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .timeout_connect(Some(Duration::from_secs(5)))
            .user_agent(format!("worlds-agent/{}", crate::VERSION))
            .build();
        Client {
            agent: config.into(),
            base: base.to_string(),
            vm_id: vm_id.to_string(),
            key,
        }
    }

    /// One call. `seconds` is the header's time (the server's clock as the agent knows it);
    /// `idempotency` adds `Idempotency-Key` (POSTs). `Err` is a transport failure (no answer).
    pub fn call(
        &self,
        method: &str,
        target: &str,
        body: Option<&[u8]>,
        idempotency: Option<&str>,
        seconds: u64,
    ) -> Result<Reply, ()> {
        let bytes = body.unwrap_or(b"");
        let auth = sign::header(&self.key, &self.vm_id, method, target, seconds, bytes);
        let url = format!("{}{target}", self.base);
        let result = match method {
            "GET" => self
                .agent
                .get(&url)
                .header(sign::HEADER, &auth)
                .header("Accept", "application/json")
                .call(),
            "DELETE" => self
                .agent
                .delete(&url)
                .header(sign::HEADER, &auth)
                .header("Accept", "application/json")
                .call(),
            "PUT" | "POST" => {
                let mut req = if method == "PUT" {
                    self.agent.put(&url)
                } else {
                    self.agent.post(&url)
                };
                req = req
                    .header(sign::HEADER, &auth)
                    .header("Accept", "application/json")
                    .header("Content-Type", "application/json");
                if let Some(key) = idempotency {
                    req = req.header("Idempotency-Key", key);
                }
                req.send(bytes)
            }
            _ => return Err(()),
        };
        let mut response = result.map_err(|_| ())?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|s| Duration::from_secs(s.min(3600)));
        let body = response
            .body_mut()
            .with_config()
            .limit(BODY_MAX)
            .read_to_vec()
            .map_err(|_| ())?;
        Ok(Reply {
            status,
            retry_after,
            body,
        })
    }
}
