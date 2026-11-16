//! Play session and cloud save bodies (SAVES_API.md §3-§4, §7).

use serde::{Deserialize, Serialize};

/// Progress numbers read by the server from a bundle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSummary {
    /// Credits.
    pub credits: Option<i64>,
    /// XP.
    pub xp: Option<i64>,
    /// Driver level.
    pub level: Option<i64>,
    /// Cars owned.
    pub cars: Option<i64>,
}

/// Who holds the profile's lease, as the head reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseView {
    /// The holder's PC name.
    pub device_label: String,
    /// When it lapses unless renewed.
    pub expires_at: String,
    /// Whether it held at the time of the read.
    pub live: bool,
}

/// `GET .../save` (ETag `"<revision>"`). An empty line is `revision: 0` with the rest null.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveHead {
    /// Head revision, 0 when empty.
    pub revision: u64,
    /// Content digest (hex).
    pub content_sha256: Option<String>,
    /// Bundle digest (hex).
    pub blob_sha256: Option<String>,
    /// Bundle size.
    pub bytes: Option<u32>,
    /// When the head was committed.
    pub committed_at: Option<String>,
    /// fi-saves `CommitKind`.
    pub kind: Option<String>,
    /// PC that wrote it (player uploads).
    pub device_label: Option<String>,
    /// Its numbers.
    pub summary: Option<SaveSummary>,
    /// Current holder, if any.
    pub lease: Option<LeaseView>,
    /// OD-52 renewal: the line lapsed, and the Supporter coverage it last saw ended at this
    /// instant. This head is the cloud save as of the lapse: once the profile is a Supporter's
    /// Hardcore again (its admission carries a lease), the launcher restores it and keeps any
    /// local progress made since only as a local backup (the server never accepts it into the
    /// line). Null otherwise; cleared by the line's next player commit (SAVES_API.md §4.7).
    #[serde(default)]
    pub lapsed_since: Option<String>,
    /// CR-S51: how a Hardcore line at revision 0 starts (SAVES_API.md §4.8). Sent only as
    /// `fresh_start` (the launcher archives the profile's folder, the game creates a new career
    /// and the first sealed revision is committed as kind `fresh_start`); `seed` is omitted, so a
    /// seeded line's head is the wire of servers before CR-S51 (absent: `seed`). Meaningless past
    /// revision 0.
    #[serde(default, skip_serializing_if = "SaveStart::is_seed")]
    pub start: SaveStart,
    /// Retention (SAVES_API.md §8): `Some(false)` when the head's save file is no longer kept
    /// (a line retired or lapsed for more than 90 days); the head cannot be downloaded or restored,
    /// so the launcher keeps the local save and says the cloud copy expired. Omitted otherwise
    /// (absent = the file is there: every server before retention).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<bool>,
}

/// `SaveHead.start` (CR-S51, SAVES_API.md §4.8): where a new Hardcore line's revision 1 comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveStart {
    /// The server's seed (`POST .../save/seed`, OD-60): every server before CR-S51.
    #[default]
    Seed,
    /// Azimuth's own first save, committed by the launcher as kind `fresh_start` and verified by
    /// the server on receipt (`SAVES.GATE_REFUSED {rule: "NOT_FRESH_START"}` otherwise).
    FreshStart,
}

impl SaveStart {
    /// Whether this is the default, omitted on the wire.
    pub fn is_seed(&self) -> bool {
        *self == SaveStart::Seed
    }
}

/// One revision in history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Revision {
    /// Its number.
    pub revision: u64,
    /// Content digest.
    pub content_sha256: String,
    /// Bundle size.
    pub bytes: u32,
    /// Commit time.
    pub committed_at: String,
    /// fi-saves `CommitKind`.
    pub kind: String,
    /// Writing PC.
    pub device_label: Option<String>,
    /// Its numbers.
    pub summary: SaveSummary,
    /// For restores.
    pub restored_from: Option<u64>,
}

/// `GET .../save/revisions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionPage {
    /// Newest first.
    pub items: Vec<Revision>,
    /// `before` for the next page.
    pub next: Option<u64>,
}

/// `PUT .../save` → 201 (or 200 with `X-Fi-Replayed: true`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitResponse {
    /// The new head.
    pub revision: u64,
    /// Content digest.
    pub content_sha256: String,
    /// Bundle digest.
    pub blob_sha256: String,
    /// Commit time.
    pub committed_at: String,
    /// Numbers read by the server.
    pub summary: SaveSummary,
}

/// `POST .../save/check` → 200 (CR-S52, SAVES_API.md §4.10): the dry run of a source profile's
/// save as this Hardcore line's start. Nothing is committed; the check is recorded by digest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedCheck {
    /// Whether a `seed_from_profile` commit of these bytes would be accepted now.
    pub accepted: bool,
    /// Why not (stable codes: `LIMITS`, `SUMMARY_UNREADABLE`, `PLAYERDB_UNREADABLE`,
    /// `FIELD_MISSING`, a fact check's code, a ceiling's code); empty when accepted.
    pub reasons: Vec<String>,
    /// The fact checks' anomaly score (0 to 100).
    pub score: u8,
    /// The play time the career was measured over, in hours (the source profile's time recorded
    /// on this server).
    pub hours: f64,
    /// The bundle's SHA-256 (hex) the check recorded.
    pub blob_sha256: String,
    /// The numbers read by the server.
    pub summary: SaveSummary,
}

/// `POST .../save/upload-check` → 200 (CR-S71, SAVES_API.md §4.11): the dry run of a player's
/// `upload` on top of `If-Match`, with every check the commit runs after verification. Nothing is
/// stored, parked or recorded. A refusal before verification answers the commit's error instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadCheck {
    /// Whether `PUT .../save` of these bytes as an `upload` on that head would be committed now.
    pub pass: bool,
    /// The error code the commit would answer (`SAVES.GATE_REFUSED`, `SAVES.SUMMARY_UNREADABLE`,
    /// `SAVES.BUNDLE_INVALID`); `null` when it passes.
    pub code: Option<String>,
    /// `SAVES.GATE_REFUSED`'s `rule` (SAVES_API.md §6); `null` otherwise.
    pub rule: Option<String>,
    /// The commit's refusal message (English); `null` when it passes.
    pub reason: Option<String>,
    /// The fact checks' score when they decided the refusal (as the commit's details).
    pub score: Option<u8>,
    /// The refusal's reason codes (as the commit's details); empty when it passes.
    pub reasons: Vec<String>,
    /// The bundle's SHA-256 (hex).
    pub blob_sha256: String,
    /// The numbers the revision would record; `null` unless it passes.
    pub summary: Option<SaveSummary>,
}

/// The play session's save lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lease {
    /// The fencing token every write must carry.
    pub fence_token: u64,
    /// Renewal length (300000).
    pub ttl_ms: u32,
    /// Heartbeat interval (60000).
    pub heartbeat_every_ms: u32,
    /// When it lapses unless renewed.
    pub expires_at: String,
}

/// `POST /launcher/v1/sessions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaySessionCreate {
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
    /// SHA-256 of the installed hook DLL.
    pub hook_sha256: String,
    /// SHA-256 of `gamedbRC.slt`.
    pub car_db_sha256: String,
    /// The revision the launcher synced (logged).
    pub base_revision: u64,
    /// Revoke a session open on another PC.
    pub take_over: bool,
}

/// The launch ticket the launcher hands to the hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchTicket {
    /// Base64 of the canonical claims JSON (ACCOUNTS.md §4).
    pub claims: String,
    /// Base64 of the Ed25519 signature over `"fi-launch-ticket-v1\n" ‖ claims`.
    pub signature: String,
}

/// Where the session plays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    /// `vip`, `free`, `legacy-free`.
    pub name: String,
    /// Tier at admission.
    pub tier: String,
    /// Base URL of the game plane for the hook.
    pub server: String,
}

/// Queue position of a waiting session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Queue {
    /// 1 = next.
    pub position: u32,
    /// Estimate from the last 15 minutes of admissions.
    pub eta_seconds: Option<u32>,
    /// When to poll again.
    pub retry_after_seconds: u32,
}

/// `POST /launcher/v1/sessions` (201 active, 202 queued) and `GET .../{id}` (200).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
#[allow(
    clippy::large_enum_variant,
    reason = "a response body built once per request and serialized at once; boxing would only add an allocation"
)]
pub enum PlaySession {
    /// Admitted, lease held.
    Active {
        /// The session.
        session_id: String,
        /// For the hook.
        ticket: LaunchTicket,
        /// For the hook's game requests (`X-Fi-Session`).
        session_token: String,
        /// Its expiry.
        session_token_expires_at: String,
        /// The save lease.
        lease: Lease,
        /// Where to play.
        pool: Pool,
        /// The cloud head at admission.
        save: SaveHead,
    },
    /// Waiting for a slot.
    Queued {
        /// The session.
        session_id: String,
        /// Position.
        queue: Queue,
    },
}

/// `POST .../heartbeat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Heartbeat {
    /// The token the launcher holds.
    pub fence_token: u64,
}

/// Whether the game plane still serves the session (SAVES_API.md §3.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GamePlane {
    /// `false` once the session is past its game-plane end: tell the hook to go offline; the
    /// save lease still holds, so checkpoints and the final commit continue.
    pub open: bool,
    /// The end: no session token is valid past it.
    pub until: String,
    /// Why it ends before the session's maximum age: `vip_ended`, `revoked`, `suspended`; or
    /// `max_age`.
    pub reason: Option<String>,
}

/// `POST .../heartbeat` → 200. `lease.fence_token` changes when a lapsed lease was re-acquired.
/// (`POST .../token` answers `{sessionToken, sessionTokenExpiresAt}` alone.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatResponse {
    /// The lease now.
    pub lease: Lease,
    /// A session token for the hook; its expiry never passes `game_plane.until`, so once the game
    /// plane closed it is the last one, already expired.
    pub session_token: String,
    /// Its expiry.
    pub session_token_expires_at: String,
    /// The game plane's status (absent from servers before SER-M2's max-age rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_plane: Option<GamePlane>,
}

/// Why a session ends, as the launcher reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// `azimuth.exe` exited.
    GameExit,
    /// The player stopped it from the launcher.
    Player,
}

/// `POST .../end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EndSession {
    /// The token the launcher holds.
    pub fence_token: u64,
    /// Why.
    pub reason: EndReason,
    /// Head after the final commit.
    pub end_revision: Option<u64>,
}

/// `POST .../installed`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Installed {
    /// The revision installed.
    pub revision: u64,
    /// Its content digest as the launcher verified it.
    pub content_sha256: String,
}

/// `POST .../save/restore`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Restore {
    /// Revision to bring back.
    pub from_revision: u64,
    /// Head the player saw.
    pub expected_revision: u64,
    /// The caller's live session, if any.
    pub session_id: Option<String>,
    /// Its token.
    pub fence_token: Option<u64>,
}

/// A parked bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Parked {
    /// Its id.
    pub parked_id: String,
    /// `conflict_loser`, `gate_refused`, `unreadable`, `pre_restore`, `pre_reset`.
    pub reason: String,
    /// Head it was based on.
    pub based_on: Option<u64>,
    /// Its numbers when readable.
    pub summary: Option<SaveSummary>,
    /// Why, in words.
    pub detail: String,
    /// When.
    pub parked_at: String,
    /// When retention drops it.
    pub expires_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn play_session_bodies_are_tagged_by_state() {
        let queued = PlaySession::Queued {
            session_id: "s".into(),
            queue: Queue {
                position: 3,
                eta_seconds: Some(90),
                retry_after_seconds: 10,
            },
        };
        let json = serde_json::to_value(&queued).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"state": "queued", "sessionId": "s", "queue": {"position": 3, "etaSeconds": 90, "retryAfterSeconds": 10}})
        );
        let active = PlaySession::Active {
            session_id: "s".into(),
            ticket: LaunchTicket {
                claims: "e30=".into(),
                signature: "AA==".into(),
            },
            session_token: "v4.public.x".into(),
            session_token_expires_at: "2026-10-17T14:15:00.000000Z".into(),
            lease: Lease {
                fence_token: 7,
                ttl_ms: 300_000,
                heartbeat_every_ms: 60_000,
                expires_at: "2026-10-17T14:05:00.000000Z".into(),
            },
            pool: Pool {
                name: "vip".into(),
                tier: "vip".into(),
                server: "https://vip.example".into(),
            },
            save: SaveHead::default(),
        };
        let back: PlaySession =
            serde_json::from_value(serde_json::to_value(&active).unwrap()).unwrap();
        assert_eq!(back, active);
        assert_eq!(
            serde_json::to_value(&active).unwrap()["sessionTokenExpiresAt"],
            "2026-10-17T14:15:00.000000Z"
        );
    }

    #[test]
    fn requests_refuse_unknown_fields() {
        let ok = r#"{"sessionId":"s","game":"fh2","xuid":"2535100000000001","nonce":"00","launcherVersion":"0.1.0","hookSha256":"h","carDbSha256":"c","baseRevision":0,"takeOver":false}"#;
        assert!(serde_json::from_str::<PlaySessionCreate>(ok).is_ok());
        let extra = ok.replace("\"takeOver\":false", "\"takeOver\":false,\"tier\":\"vip\"");
        assert!(serde_json::from_str::<PlaySessionCreate>(&extra).is_err());
    }
}
