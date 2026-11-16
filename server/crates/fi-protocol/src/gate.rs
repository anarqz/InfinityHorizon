//! DTOs of `/gate/v1` (docs/contracts/GATE_API.md; platform v1, ADR 0015): the admission
//! request, the queued and active answers, the heartbeat and leave bodies, the public queue
//! summary and the internal pool list. The ticket itself is `fi_auth::session` (the session token
//! of ACCOUNTS.md §4.2, now the game access ticket of GATE_API.md §3); [`Ticket`] is its wire
//! form. Mirror: `packages/contracts/src/gate.ts`, names prefixed `Gate` (the test below keeps the
//! field names equal).
//! Routes: [`crate::routes::gate`].

use crate::saves::{GamePlane, LaunchTicket, Lease, Pool, SaveHead};
use serde::{Deserialize, Serialize};

/// SAVES_API.md §3.1's body (`game` names the title) without OD-109's `dedicated`: what the
/// frozen Rust launcher (`crates/fi-launcher-core`) sends and the `/launcher/v1/sessions` alias
/// reads (it refuses `dedicated` as an unknown field). The gate's route reads [`SessionAdmission`].
pub type SessionCreate = crate::saves::PlaySessionCreate;

/// `POST /gate/v1/sessions` (TypeScript `GateSessionCreate`): [`SessionCreate`] plus OD-109's
/// optional `dedicated` (a dedicated lobby's host session, GATE_API.md §2 "Dedicated
/// sessions"), for which `carDbSha256` may be absent. A body without `dedicated` is exactly
/// [`SessionCreate`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionAdmission {
    /// Minted by the launcher (UUIDv7); also the `Idempotency-Key`.
    pub session_id: String,
    /// `fh2`.
    pub game: String,
    /// Profile XUID, decimal.
    pub xuid: String,
    /// 32 hex digits: the handoff nonce echoed into the launch ticket.
    pub nonce: String,
    /// Launcher version.
    pub launcher_version: String,
    /// SHA-256 of the installed hook DLL (a dedicated session: the SHA-256 of zero bytes).
    pub hook_sha256: String,
    /// SHA-256 of `gamedbRC.slt` (empty: none declared). Required unless `dedicated` (the gate
    /// answers 400 `REQUEST.INVALID {field: "carDbSha256"}`); a dedicated session may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car_db_sha256: Option<String>,
    /// The revision the launcher synced (logged).
    pub base_revision: u64,
    /// Revoke a session open on another PC.
    pub take_over: bool,
    /// A dedicated lobby's host session (OD-109): admins only, no game, no save lease, outside
    /// capacity. Absent: `false` (an older launcher's body is unchanged).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dedicated: bool,
    /// A modded session (OD-112 follow-up, GATE_API.md §2 "Modded sessions"): its multiplayer
    /// mod set is not empty. Self-declared from the launch plan: compatibility evidence, not
    /// anti-cheat. Matchmaking treats it as non-vanilla; boards and the universe record none of
    /// its results. Absent: `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub modded: bool,
    /// What this client can do (MATCHMAKING_API.md §2.10, CR-S96): at most
    /// [`MAX_SESSION_FEATURES`] names, unknown ones ignored. Absent: none (≤ 2.0.4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
}

/// The capability of a client that handles private official worlds (WORLDS_OFFICIAL.md §12.8).
pub const FEATURE_PRIVATE_OFFICIAL: &str = "private_official_v1";

/// The most names of [`SessionAdmission::features`].
pub const MAX_SESSION_FEATURES: usize = 16;

impl SessionAdmission {
    /// The body without `dedicated`: what the checks shared with the `/launcher/v1` alias read.
    pub fn play(&self) -> crate::saves::PlaySessionCreate {
        crate::saves::PlaySessionCreate {
            session_id: self.session_id.clone(),
            game: self.game.clone(),
            xuid: self.xuid.clone(),
            nonce: self.nonce.clone(),
            launcher_version: self.launcher_version.clone(),
            hook_sha256: self.hook_sha256.clone(),
            car_db_sha256: self.car_db_sha256.clone().unwrap_or_default(),
            base_revision: self.base_revision,
            take_over: self.take_over,
        }
    }
}

/// The game access ticket as the launcher receives it (GATE_API.md §3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    /// PASETO `v4.public`; the hook sends it as `X-Fi-Session` on every game request.
    pub token: String,
    /// Its `exp`: `min(iat + 10 min, gameUntil)`.
    pub expires_at: String,
    /// When to heartbeat for the next one (60000).
    pub heartbeat_every_ms: u32,
}

/// A queued session's place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuePlace {
    /// 1 = next.
    pub position: u32,
    /// Sessions in line before this one (`position - 1`).
    pub ahead: u32,
    /// Estimate from the last 15 minutes of admissions, when there were any.
    pub eta_seconds: Option<u32>,
    /// When to poll again (10 to 30 s); 90 s unpolled leaves the queue.
    pub retry_after_seconds: u32,
}

/// `POST /gate/v1/sessions` (201 active, 202 queued) and `GET /gate/v1/sessions/{id}` (200).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[allow(
    clippy::large_enum_variant,
    reason = "a response body built once per request and serialized at once"
)]
pub enum GateSession {
    /// Admitted: the slot (and, for a session that keeps a save line, the save lease) is held.
    Active {
        /// The session.
        session_id: String,
        /// The game.
        game: String,
        /// Where to play: `server` is the saves origin (`vip`) or the matchmaking origin (free).
        pool: Pool,
        /// The game access ticket.
        ticket: Ticket,
        /// ACCOUNTS.md §4.2's launch ticket for the pipe handoff.
        launch_ticket: LaunchTicket,
        /// The save lease: only for a session that keeps a save line (its profile's tier and mode
        /// in the server's `FI_CLOUD_SAVES`, CR-S51; OD-52's Supporter Hardcore by default),
        /// whatever the pool.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lease: Option<Lease>,
        /// The cloud head at admission: with `lease` only.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        save: Option<SaveHead>,
        /// Until when game surfaces serve the session.
        game_until: String,
        /// Admission + 12 h: heartbeats past it answer `SESSIONS.ENDED {reason: max_age}`.
        max_until: String,
    },
    /// Waiting for a slot.
    Queued {
        /// The session.
        session_id: String,
        /// The game.
        game: String,
        /// The pool it waits for.
        pool: Pool,
        /// Its place.
        queue: QueuePlace,
    },
}

/// `POST /gate/v1/sessions/{id}/heartbeat`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Heartbeat {
    /// The lease's fencing token: required for a session that holds the save lease (its
    /// admission carried `lease`, CR-S51), absent otherwise.
    #[serde(default)]
    pub fence_token: Option<u64>,
}

/// `POST .../heartbeat` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatResponse {
    /// The save lease now (a session with a save line; a new `fenceToken` after a resumed lapse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<Lease>,
    /// A fresh ticket; its expiry never passes `gamePlane.until`.
    pub ticket: Ticket,
    /// Whether game surfaces still serve the session.
    pub game_plane: GamePlane,
}

/// `POST .../ticket` → 200: a fresh ticket without renewing the lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketResponse {
    /// The ticket.
    pub ticket: Ticket,
}

/// Why the launcher leaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaveReason {
    /// The game exited.
    GameExit,
    /// The player stopped it from the launcher.
    Player,
}

/// `POST /gate/v1/sessions/{id}/leave` → 204 (idempotent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Leave {
    /// Why.
    pub reason: LeaveReason,
    /// The head after the final commit (`vip`).
    #[serde(default)]
    pub end_revision: Option<u64>,
    /// The lease's fencing token, when the launcher holds one; without it the gate releases the
    /// lease this session holds.
    #[serde(default)]
    pub fence_token: Option<u64>,
}

/// One pool of `GET /gate/v1/queue`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolQueue {
    /// `vip`, `matchmaking`.
    pub name: String,
    /// `vip` | `free`.
    pub tier: String,
    /// Live sessions.
    pub active: u32,
    /// Waiting sessions.
    pub queued: u32,
    /// Slots (`null`: unlimited).
    pub capacity: Option<u32>,
    /// Whether a launch now is admitted or queued (false in maintenance).
    pub accepting: bool,
    /// Estimated wait of a launch now.
    pub eta_seconds: Option<u32>,
}

/// The game's online capacity and totals over its pools (OD-71, GATE_API.md §6).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineQueue {
    /// The most active sessions of the game at once (`null`: unlimited).
    pub capacity: Option<u32>,
    /// Live sessions, every pool.
    pub active: u32,
    /// Waiting sessions, every pool.
    pub queued: u32,
}

/// `GET /gate/v1/queue?game=` → 200 (`Cache-Control: public, max-age=10`; no identities).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSummary {
    /// The game.
    pub game: String,
    /// Its pools.
    pub pools: Vec<PoolQueue>,
    /// The game's online capacity and totals (OD-71; a server from before it sends none, read as
    /// unlimited and zero).
    #[serde(default)]
    pub online: OnlineQueue,
    /// While the game is in maintenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance_until: Option<String>,
    /// When the numbers were read.
    pub generated_at: String,
}

/// One pool of `GET /gate/v1/internal/pools` (GAME_PLATFORM.md §3.1 with `game`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatus {
    /// The game.
    pub game: String,
    /// Name.
    pub name: String,
    /// `vip` | `free`.
    pub tier: String,
    /// Admission capacity.
    pub capacity: Option<u32>,
    /// Longest queue.
    pub queue_limit: u32,
    /// Whether it admits.
    pub accepting: bool,
    /// Live sessions per instance the autoscaler plans with (`FI_POOLS_JSON`).
    pub instance_capacity: Option<u32>,
    /// Live active sessions.
    pub active: u32,
    /// Waiting sessions.
    pub queued: u32,
    /// The origin the hook talks to.
    pub game_endpoint: String,
}

/// `GET /gate/v1/internal/pools` → 200.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pools {
    /// Every pool of every game.
    pub pools: Vec<PoolStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn ticket() -> Ticket {
        Ticket {
            token: "v4.public.x".into(),
            expires_at: "2026-10-19T12:10:00.000000Z".into(),
            heartbeat_every_ms: 60_000,
        }
    }

    fn pool() -> Pool {
        Pool {
            name: "matchmaking".into(),
            tier: "free".into(),
            server: "https://mm.example".into(),
        }
    }

    fn samples() -> Vec<(&'static str, Value)> {
        let active = GateSession::Active {
            session_id: "s".into(),
            game: "fh2".into(),
            pool: pool(),
            ticket: ticket(),
            launch_ticket: LaunchTicket {
                claims: "e30=".into(),
                signature: "AA==".into(),
            },
            lease: Some(Lease {
                fence_token: 7,
                ttl_ms: 300_000,
                heartbeat_every_ms: 60_000,
                expires_at: "2026-10-19T12:05:00.000000Z".into(),
            }),
            save: Some(SaveHead::default()),
            game_until: "2026-10-20T00:00:00.000000Z".into(),
            max_until: "2026-10-20T00:00:00.000000Z".into(),
        };
        let queued = GateSession::Queued {
            session_id: "s".into(),
            game: "fh2".into(),
            pool: pool(),
            queue: QueuePlace {
                position: 3,
                ahead: 2,
                eta_seconds: Some(90),
                retry_after_seconds: 10,
            },
        };
        let hb = HeartbeatResponse {
            lease: None,
            ticket: ticket(),
            game_plane: GamePlane {
                open: true,
                until: "2026-10-20T00:00:00.000000Z".into(),
                reason: Some("max_age".into()),
            },
        };
        let summary = QueueSummary {
            game: "fh2".into(),
            pools: vec![PoolQueue {
                name: "vip".into(),
                tier: "vip".into(),
                active: 1,
                queued: 0,
                capacity: None,
                accepting: true,
                eta_seconds: Some(0),
            }],
            online: OnlineQueue {
                capacity: Some(50),
                active: 1,
                queued: 0,
            },
            maintenance_until: Some("2026-10-19T13:00:00.000000Z".into()),
            generated_at: "2026-10-19T12:00:00.000000Z".into(),
        };
        let pools = Pools {
            pools: vec![PoolStatus {
                game: "fh2".into(),
                name: "vip".into(),
                tier: "vip".into(),
                capacity: Some(12),
                queue_limit: 100,
                accepting: true,
                instance_capacity: Some(4),
                active: 0,
                queued: 0,
                game_endpoint: "http://127.0.0.1:8480".into(),
            }],
        };
        let leave = Leave {
            reason: LeaveReason::GameExit,
            end_revision: Some(41),
            fence_token: Some(7),
        };
        vec![
            ("GateActive", serde_json::to_value(active).unwrap()),
            ("GateQueued", serde_json::to_value(queued).unwrap()),
            ("GateHeartbeatResponse", serde_json::to_value(hb).unwrap()),
            (
                "GateTicketResponse",
                serde_json::to_value(TicketResponse { ticket: ticket() }).unwrap(),
            ),
            ("GateQueueSummary", serde_json::to_value(summary).unwrap()),
            ("GatePools", serde_json::to_value(pools).unwrap()),
            ("GateLeave", serde_json::to_value(leave).unwrap()),
            (
                "GateHeartbeat",
                serde_json::to_value(Heartbeat {
                    fence_token: Some(1),
                })
                .unwrap(),
            ),
            ("GateSessionCreate", json!(dedicated_create())),
        ]
    }

    fn dedicated_create() -> SessionAdmission {
        SessionAdmission {
            session_id: "s".into(),
            game: "fh2".into(),
            xuid: "2535100000000001".into(),
            nonce: "00".into(),
            launcher_version: "1.0.9".into(),
            hook_sha256: "e3".into(),
            car_db_sha256: None,
            base_revision: 0,
            take_over: false,
            dedicated: true,
            modded: true,
            features: vec![FEATURE_PRIVATE_OFFICIAL.into()],
        }
    }

    /// OD-109: `dedicated` and an absent `carDbSha256` are additive on `/gate/v1`; an older
    /// launcher's body reads as an ordinary session and serializes byte-identically; the
    /// `/launcher/v1` alias body refuses `dedicated` as an unknown field.
    #[test]
    fn dedicated_is_optional_on_the_gate_and_unknown_on_the_alias() {
        let old = r#"{"sessionId":"s","game":"fh2","xuid":"2535100000000001","nonce":"00","launcherVersion":"0.1.0","hookSha256":"h","carDbSha256":"c","baseRevision":0,"takeOver":false}"#;
        let parsed: SessionAdmission = serde_json::from_str(old).unwrap();
        assert!(!parsed.dedicated);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::from_str::<Value>(old).unwrap()
        );
        assert_eq!(
            serde_json::to_value(parsed.play()).unwrap(),
            serde_json::from_str::<Value>(old).unwrap()
        );
        let dedicated = r#"{"sessionId":"s","game":"fh2","xuid":"2535100000000001","nonce":"00","launcherVersion":"0.1.0","hookSha256":"h","baseRevision":0,"takeOver":false,"dedicated":true}"#;
        let parsed: SessionAdmission = serde_json::from_str(dedicated).unwrap();
        assert!(parsed.dedicated);
        assert!(!parsed.modded);
        assert_eq!(parsed.car_db_sha256, None);
        // OD-112 follow-up: `modded` is additive on the gate and unknown on the alias.
        let modded = old.replace("\"takeOver\":false", "\"takeOver\":false,\"modded\":true");
        assert!(
            serde_json::from_str::<SessionAdmission>(&modded)
                .unwrap()
                .modded
        );
        assert!(serde_json::from_str::<SessionCreate>(&modded).is_err());
        assert_eq!(parsed.play().car_db_sha256, "");
        assert!(
            serde_json::from_str::<SessionCreate>(&old.replace(
                "\"takeOver\":false",
                "\"takeOver\":false,\"dedicated\":false"
            ))
            .is_err()
        );
        assert!(
            serde_json::from_str::<SessionAdmission>(
                &old.replace("\"takeOver\":false", "\"takeOver\":false,\"tier\":\"vip\"")
            )
            .is_err()
        );
    }

    #[test]
    fn bodies_are_tagged_camel_case_and_requests_strict() {
        let s = samples();
        assert_eq!(s[0].1["state"], "active");
        assert_eq!(s[0].1["launchTicket"]["claims"], "e30=");
        assert_eq!(s[0].1["ticket"]["heartbeatEveryMs"], 60_000);
        assert_eq!(
            s[1].1["queue"],
            json!({"position": 3, "ahead": 2, "etaSeconds": 90, "retryAfterSeconds": 10})
        );
        // A free session's body has no lease and no save.
        let free = GateSession::Active {
            session_id: "s".into(),
            game: "fh2".into(),
            pool: pool(),
            ticket: ticket(),
            launch_ticket: LaunchTicket {
                claims: "e30=".into(),
                signature: "AA==".into(),
            },
            lease: None,
            save: None,
            game_until: "x".into(),
            max_until: "x".into(),
        };
        let v = serde_json::to_value(&free).unwrap();
        assert!(v.get("lease").is_none() && v.get("save").is_none());
        assert_eq!(serde_json::from_value::<GateSession>(v).unwrap(), free);
        assert_eq!(
            serde_json::from_str::<Heartbeat>("{}").unwrap().fence_token,
            None
        );
        assert!(serde_json::from_str::<Heartbeat>(r#"{"fenceToken":1,"x":1}"#).is_err());
        let leave: Leave = serde_json::from_str(r#"{"reason":"player"}"#).unwrap();
        assert_eq!(leave.reason, LeaveReason::Player);
        assert!(serde_json::from_str::<Leave>(r#"{"reason":"crash"}"#).is_err());
    }
}
