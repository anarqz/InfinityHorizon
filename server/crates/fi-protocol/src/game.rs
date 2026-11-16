//! Game-plane platform bodies and the internal surface of the local environment v0
//! (docs/contracts/GAME_PLATFORM.md): `/game/v1` (session, progression, presence), the response
//! headers that say which pool and instance answered, `/internal/v1/pools` and the local
//! entitlement stand-in; plus the token-rotation answer of SAVES_API.md §3.3. Mirror:
//! `packages/contracts/src/game.ts`.

use serde::{Deserialize, Serialize};

/// Response header on every game-plane answer: the pool the deployment serves (`FI_POOL`).
pub const FI_POOL: &str = "x-fi-pool";
/// Response header on every game-plane answer: the instance that answered (`FI_INSTANCE_ID`).
pub const FI_INSTANCE: &str = "x-fi-instance";

/// `kind` of a progression fact (GAME_PLATFORM.md §2.2).
pub const PROGRESSION_KINDS: [&str; 7] = [
    "race_result",
    "score",
    "xp",
    "profile_sync",
    "tables_sync",
    "drivatar",
    "livery",
];

/// `GET /game/v1/session` → 200: the session as the token and Postgres see it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameSession {
    /// The play session.
    pub session_id: String,
    /// Its account.
    pub account_id: String,
    /// Its profile.
    pub profile_id: String,
    /// The profile's XUID, decimal.
    pub xuid: String,
    /// The pool (always this deployment's).
    pub pool: String,
    /// The tier the token was minted with.
    pub tier: String,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// Until when the game plane serves the session.
    pub game_until: String,
    /// The instance that answered.
    pub instance: String,
}

/// `POST /game/v1/progression` body (header `X-Fh2-Request-Id` required).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgressionFact {
    /// One of [`PROGRESSION_KINDS`].
    pub kind: String,
    /// Kind-specific facts (GAME_PLATFORM.md §2.2).
    pub data: serde_json::Map<String, serde_json::Value>,
}

/// `POST /game/v1/progression` → 201 (`recorded: true`) or 200 (a retry: `recorded: false`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressionRecorded {
    /// `UUIDv5(profileId, X-Fh2-Request-Id)`: the fact's id (`progression_event.event_id`).
    pub event_id: String,
    /// Whether this request wrote it.
    pub recorded: bool,
    /// When the first copy was received.
    pub recorded_at: String,
}

/// One pool of `GET /internal/v1/pools`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatus {
    /// `pool.name`.
    pub name: String,
    /// `free` | `vip`.
    pub tier: String,
    /// Admission capacity (`null`: unlimited).
    pub capacity: Option<u32>,
    /// Longest queue.
    pub queue_limit: u32,
    /// Whether it admits.
    pub accepting: bool,
    /// Live sessions per instance the autoscaler plans with (`FI_POOLS_JSON`; `null` when absent).
    pub instance_capacity: Option<u32>,
    /// Live active sessions (database clock).
    pub active: u32,
    /// Queued sessions polled within 90 s.
    pub queued: u32,
    /// The game plane's base URL.
    pub game_endpoint: String,
}

/// `GET /internal/v1/pools` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pools {
    /// Every pool row, by name.
    pub pools: Vec<PoolStatus>,
}

/// `PUT /internal/v1/local/entitlements` body (local only).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalEntitlement {
    /// The account.
    pub account_id: String,
    /// `active` | `grace` | `expired` | `none`.
    pub status: String,
    /// End of access (`null` for none).
    pub access_until: Option<String>,
    /// `comp`, optional.
    #[serde(default)]
    pub plan: Option<String>,
}

/// `PUT /internal/v1/local/entitlements` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalEntitlementResult {
    /// The account.
    pub account_id: String,
    /// `free` | `vip` by the ACCOUNTS.md §7 rule at the database clock.
    pub tier: String,
    /// The stored status.
    pub status: String,
    /// The stored end of access.
    pub access_until: Option<String>,
}

/// `POST /launcher/v1/sessions/{id}/token` → 200 (SAVES_API.md §3.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokenGrant {
    /// A fresh session token for the hook.
    pub session_token: String,
    /// Its expiry (never past the game-plane end).
    pub session_token_expires_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_camel_case_and_requests_strict() {
        let fact: ProgressionFact =
            serde_json::from_str(r#"{"kind":"xp","data":{"amount":5,"accepted":true}}"#).unwrap();
        assert_eq!(fact.kind, "xp");
        assert!(
            serde_json::from_str::<ProgressionFact>(r#"{"kind":"xp","data":{},"extra":1}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<ProgressionFact>(r#"{"kind":"xp","data":[]}"#).is_err());
        let e: LocalEntitlement = serde_json::from_str(
            r#"{"accountId":"a","status":"active","accessUntil":"2026-10-18T00:00:00.000000Z"}"#,
        )
        .unwrap();
        assert_eq!(e.plan, None);
        let pools = Pools {
            pools: vec![PoolStatus {
                name: "vip".into(),
                tier: "vip".into(),
                capacity: Some(12),
                queue_limit: 100,
                accepting: true,
                instance_capacity: Some(4),
                active: 0,
                queued: 0,
                game_endpoint: "http://127.0.0.1:8480/pools/vip".into(),
            }],
        };
        let json = serde_json::to_value(&pools).unwrap();
        assert_eq!(json["pools"][0]["instanceCapacity"], 4);
        assert_eq!(json["pools"][0]["queueLimit"], 100);
        assert_eq!(
            json["pools"][0]["gameEndpoint"],
            "http://127.0.0.1:8480/pools/vip"
        );
    }
}
