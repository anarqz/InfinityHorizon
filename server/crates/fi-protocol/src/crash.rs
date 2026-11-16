//! Crash intake v1, `docs/contracts/CRASH_API.md`. All client evidence is untrusted.
use serde::{Deserialize, Serialize};

/// Explicit, per-incident upload permissions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrashConsent {
    /// Required consent to send metadata.
    pub metadata: bool,
    /// Legacy property: must be false; dump intake is disabled (OD-78).
    pub minidump: bool,
}
/// Server-verifiable incident context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrashIncident {
    /// Client clock, checked against server membership history.
    pub occurred_at: String,
    /// Owned, admitted play session.
    pub session_id: String,
    /// Optional world, never a client roster.
    pub world_id: Option<String>,
}
/// Bounded build labels, not remote attestation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrashBuild {
    /// `fh2` in v1.
    pub game: String,
    /// Game build label.
    pub game_build: String,
    /// Launcher version.
    pub launcher_version: String,
    /// Hook version.
    pub hook_version: String,
}
/// Crash versus an abnormal process exit.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashKind {
    /// Exception observed locally.
    Exception,
    /// Non-normal exit.
    AbnormalExit,
}
/// Allowlisted module category; never a path.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashModule {
    /// Game image.
    Game,
    /// Infinity hook.
    Hook,
    /// Infinity overlay.
    Overlay,
    /// Unresolved.
    Unknown,
}
/// Bounded diagnostic codes, not blame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrashDiagnostic {
    /// Kind of incident.
    pub kind: CrashKind,
    /// Unsigned process exit status.
    pub exit_code: Option<u32>,
    /// Unsigned exception status.
    pub exception_code: Option<u32>,
    /// Module category.
    pub fault_module: CrashModule,
    /// Module-relative hex offset, never process address.
    pub fault_offset: Option<String>,
}
/// Origin of an untrusted participant hint.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashObservationKind {
    /// In a client roster.
    Roster,
    /// Last received packet.
    LastPacket,
    /// A content reference.
    ContentReference,
}
/// Untrusted hint; never independently creates a strike.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrashObservation {
    /// Server resolves this decimal XUID; never accept a client account id.
    pub participant_xuid: String,
    /// Hint type.
    pub kind: CrashObservationKind,
    /// Optional private content digest.
    pub content_sha256: Option<String>,
}
/// Legacy descriptor recognized only to return CRASH.DUMPS_DISABLED; never authorizes upload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrashDumpDescriptor {
    /// Legacy declared length; no bytes are accepted.
    pub byte_length: u32,
    /// Full lowercase SHA-256.
    pub sha256: String,
}
/// `POST /crash/v1/reports`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CrashCreate {
    /// Exactly 1.
    pub schema_version: u8,
    /// Client-minted incident UUID.
    pub report_id: String,
    /// Explicit permissions.
    pub consent: CrashConsent,
    /// Session/world/time.
    pub incident: CrashIncident,
    /// Bounded versions.
    pub build: CrashBuild,
    /// Diagnostic codes.
    pub crash: CrashDiagnostic,
    /// At most sixteen hints.
    pub observations: Vec<CrashObservation>,
    /// Must be absent/null; any descriptor receives CRASH.DUMPS_DISABLED.
    pub dump: Option<CrashDumpDescriptor>,
}
/// Authenticated capabilities (OD-78); no dump upload capability exists.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashCapabilities {
    /// Protocol version.
    pub schema_version: u8,
    /// Metadata intake enabled.
    pub metadata_enabled: bool,
    /// Always false in this phase.
    pub minidumps_enabled: bool,
    /// Metadata body bound.
    pub max_metadata_bytes: u32,
    /// Bounded observation count.
    pub max_observations: u32,
}
/// Stable 202 response; no attribution claim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashAccepted {
    /// Incident id.
    pub report_id: String,
    /// Always `queued` at acceptance.
    pub state: String,
    /// Legacy always-null property. No usable capability may be returned.
    pub dump_upload: Option<()>,
}
/// Private status of the authenticated account's report.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashView {
    /// Incident id.
    pub report_id: String,
    /// `queued` or `analyzed`.
    pub state: String,
    /// `unknown`, `needs_review` or `reviewed`; workflow only, never blame.
    pub classification: String,
    /// Always `disabled` in this phase.
    pub dump_status: String,
    /// Metadata expiry, UTC.
    pub retained_until: String,
}
