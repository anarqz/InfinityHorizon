//! OD-79: a short allowlisted original-game startup hash check, not a filesystem inventory.
use serde::{Deserialize, Serialize};
/// Process enforcement; bypass is exclusively for explicitly local synthetic tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentEnforcement {
    /// Required.
    Required,
    /// Explicit local test bypass.
    LocalTestBypass,
}
/// Compatibility, never malicious intent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentState {
    /// Every selected original file matches.
    Compatible,
    /// At least one known selected file differs.
    Incompatible,
    /// Missing/unreadable/incomplete observation.
    Inconclusive,
}
/// Bounded non-punitive reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentReason {
    /// Compatible.
    Compatible,
    /// No server dictionary.
    PolicyUnavailable,
    /// Dictionary changed.
    PolicyChanged,
    /// No startup scan.
    MissingScan,
    /// Startup permission expired.
    ScanExpired,
    /// Original-file digest/size differs.
    IncompatibleFiles,
    /// Selected-file read incomplete.
    InconclusiveScan,
}
/// One named file, never a directory scope or uploaded absolute path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentCriticalFile {
    /// Stable dictionary entry id.
    pub file_id: String,
    /// ASCII path relative to selected game-content root.
    pub path: String,
    /// Expected size, at most 256MiB.
    pub byte_length: u64,
}
/// Small original-file read list. Expected hashes stay in the server dictionary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentManifest {
    /// Immutable dictionary revision UUID.
    pub manifest_id: String,
    /// Game code.
    pub game: String,
    /// Reference build label.
    pub game_build: String,
    /// Two to sixteen selected files, not an inventory.
    pub files: Vec<ContentCriticalFile>,
}
/// Authenticated discovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentCapabilities {
    /// Version one.
    pub schema_version: u8,
    /// Game code.
    pub game: String,
    /// Server policy, not a client bypass flag.
    pub enforcement: ContentEnforcement,
    /// None until a reference dictionary is explicitly configured.
    pub manifest: Option<ContentManifest>,
    /// Hard cap16.
    pub max_files: u8,
    /// Hard cap8192 bytes.
    pub max_scan_bytes: u32,
}
/// One startup observation. No client-supplied path, expected digest or trusted state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContentObservedFile {
    /// Id from the current short manifest.
    pub file_id: String,
    /// Observed size, or unreadable.
    pub byte_length: Option<u64>,
    /// Full lowercase SHA-256, or unreadable.
    pub sha256: Option<String>,
}
/// Authenticated future/current session startup check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContentScan {
    /// Fresh UUIDv7; matches Idempotency-Key.
    pub scan_id: String,
    /// Client-minted future/current session UUID.
    pub session_id: String,
    /// Game code.
    pub game: String,
    /// Reference dictionary revision.
    pub manifest_id: String,
    /// Only selected file observations, at most16.
    pub files: Vec<ContentObservedFile>,
}
/// One selected file's server comparison result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentFileResult {
    /// Manifest file id.
    pub file_id: String,
    /// Exact comparison result.
    pub state: ContentState,
}
/// Current own-session online permission; no world policy or account sanction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentPermission {
    /// Future/current session id.
    pub session_id: String,
    /// Latest retained startup action.
    pub scan_id: Option<String>,
    /// Server comparison result.
    pub state: ContentState,
    /// Allows ordinary online continuation.
    pub online_allowed: bool,
    /// Non-punitive reason.
    pub reason: ContentReason,
    /// Bounded per-file results.
    pub files: Vec<ContentFileResult>,
    /// At most12h from scan acceptance.
    pub expires_at: Option<String>,
}
/// Submitted action receipt plus CURRENT permission; a retry never replays stale allowance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentScanAccepted {
    /// Submitted scan, possibly older than permission.scanId.
    pub scan_id: String,
    /// Latest authoritative result.
    pub permission: ContentPermission,
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn scan_inputs_are_strict_but_normal_responses_are_additive() {
        assert!(
            serde_json::from_value::<ContentObservedFile>(
                json!({"fileId":"x","byteLength":null,"sha256":null,"path":"private"})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<ContentScan>(json!({"scanId":"x","sessionId":"y","game":"fh2","manifestId":"z","files":[],"state":"compatible"})).is_err());
        assert!(
            serde_json::from_str::<ContentObservedFile>(
                r#"{"fileId":"x","fileId":"y","byteLength":null,"sha256":null}"#
            )
            .is_err()
        );
        assert!(serde_json::from_value::<ContentCapabilities>(json!({"schemaVersion":1,"game":"fh2","enforcement":"required","manifest":null,"maxFiles":16,"maxScanBytes":8192,"future":true})).is_ok());
    }
}
