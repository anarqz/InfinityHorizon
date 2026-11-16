//! OD-79: compare only a short selected list of original-game files. No scanner, I/O or punishment.
use fi_protocol::content::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
/// Short-list cap, not a target for inventory.
pub const MAX_FILES: usize = 16;
/// No huge media archives in this lightweight startup check.
pub const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Largest exact JSON integer. A larger observed file can be reported without hashing/reading it.
pub const MAX_OBSERVED_BYTES: u64 = 9_007_199_254_740_991;
/// Body bytes before JSON processing.
pub const SCAN_MAX: usize = 8192;
/// One startup check lasts at most the existing12-hour session maximum.
pub const TTL_MS: i64 = 12 * 60 * 60 * 1000;
/// Authoritative PRIVATE server dictionary, not a wire DTO.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dictionary {
    /// Names of only the selected files.
    pub manifest: ContentManifest,
    /// Expected original-file SHA-256 by stable file id, never accepted from a client.
    pub expected: BTreeMap<String, String>,
}
/// OD-79 four-file reference supplied by WINDOWS. Metadata only, not game bytes or an inventory.
/// A changed reference MUST use a new immutable manifest id in a reviewed private commit.
pub fn reference_dictionary(game: &str) -> Option<Dictionary> {
    if game != "fh2" {
        return None;
    }
    let entries = [
        (
            "game_executable",
            "forza_durango_release_final.exe",
            58_204_160,
            "3ec04c6163fb5614e3d351ba0570d39b846041a257715d6340eceb03ccb7f0a3",
        ),
        (
            "car_database",
            "media/stripped/gamedbRC.slt",
            5_974_016,
            "4d50013dc7705de05a074e4421679e16a3a1eda0816841861b0b7139ecf45517",
        ),
        (
            "zip_manifest",
            "media/zipmanifest.xml",
            82_452,
            "773e6ae5d5de2eab8776c757098768918d6ad5c187633b555bc5d5c567ebaec4",
        ),
        (
            "hash_manifest",
            "hash.manifest",
            28_980,
            "56943e86659f7003eb66b9b23d8a5094e79bffa5d951d5439c98deb4f2a4a7b7",
        ),
    ];
    Some(Dictionary {
        manifest: ContentManifest {
            manifest_id: "d29804e7-e851-4fbb-908f-26865825f5f1".into(),
            game: game.into(),
            game_build: "windows-reference-20261020".into(),
            files: entries
                .iter()
                .map(|(id, path, size, _)| ContentCriticalFile {
                    file_id: (*id).into(),
                    path: (*path).into(),
                    byte_length: *size,
                })
                .collect(),
        },
        expected: entries
            .iter()
            .map(|(id, _, _, sha)| ((*id).into(), (*sha).into()))
            .collect(),
    })
}
/// Bounded validation failure; never echo input values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// Malformed or oversized input.
    Shape,
    /// Unsafe/ambiguous named-file path.
    Path,
    /// Unsupported action id or first-accept age.
    Action,
}
/// Full lowercase SHA-256.
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// Exactly64 lowercase hex digits.
pub fn valid_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn uuid(s: &str) -> bool {
    Uuid::parse_str(s).is_ok_and(|v| !v.is_nil())
}
fn label(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}
fn file_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
/// Root-relative path for ONE named file; never a directory walk or inventory scope.
pub fn safe_path(path: &str) -> bool {
    if path.is_empty()
        || path.len() > 240
        || !path.is_ascii()
        || path
            .bytes()
            .any(|b| b < 32 || b == 127 || b"\\:*?<>\"|".contains(&b))
    {
        return false;
    }
    path.split('/').all(|p| {
        if p.is_empty() || p == "." || p == ".." || p.ends_with(['.', ' ']) {
            return false;
        }
        let stem = p
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end_matches(' ')
            .to_ascii_lowercase();
        !matches!(
            stem.as_str(),
            "con" | "prn" | "aux" | "nul" | "conin$" | "conout$" | "clock$"
        ) && !(stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && (b'1'..=b'9').contains(&stem.as_bytes()[3]))
    })
}
/// Validate the private reference dictionary. No authenticity is inferred from observed hashes.
pub fn validate_dictionary(d: &Dictionary) -> Result<(), Invalid> {
    let m = &d.manifest;
    if !uuid(&m.manifest_id)
        || !label(&m.game, 16)
        || !label(&m.game_build, 32)
        || !(2..=MAX_FILES).contains(&m.files.len())
        || d.expected.len() != m.files.len()
    {
        return Err(Invalid::Shape);
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for f in &m.files {
        if !file_id(&f.file_id)
            || !ids.insert(f.file_id.as_str())
            || f.byte_length > MAX_FILE_BYTES
            || !d.expected.get(&f.file_id).is_some_and(|s| valid_digest(s))
        {
            return Err(Invalid::Shape);
        }
        if !safe_path(&f.path) || !paths.insert(f.path.to_ascii_lowercase()) {
            return Err(Invalid::Path);
        }
    }
    if !ids.contains("game_executable") || !ids.contains("car_database") {
        return Err(Invalid::Shape);
    }
    for path in &paths {
        for (i, b) in path.bytes().enumerate() {
            if b == b'/' && paths.contains(&path[..i]) {
                return Err(Invalid::Path);
            }
        }
    }
    Ok(())
}
/// Strict fields; retained exact retries can be old, so age is a separate first-accept check.
pub fn validate_scan(s: &ContentScan) -> Result<(), Invalid> {
    let action = Uuid::parse_str(&s.scan_id).map_err(|_| Invalid::Shape)?;
    if action.get_version_num() != 7
        || action.get_variant() != uuid::Variant::RFC4122
        || !uuid(&s.session_id)
        || !uuid(&s.manifest_id)
        || !label(&s.game, 16)
        || s.files.len() > MAX_FILES
        || serde_json::to_vec(s).map_err(|_| Invalid::Shape)?.len() > SCAN_MAX
    {
        return Err(Invalid::Shape);
    }
    let mut ids = BTreeSet::new();
    for f in &s.files {
        if !file_id(&f.file_id)
            || !ids.insert(&f.file_id)
            || f.byte_length.is_some_and(|n| n > MAX_OBSERVED_BYTES)
            || f.sha256.as_deref().is_some_and(|h| !valid_digest(h))
        {
            return Err(Invalid::Shape);
        }
    }
    Ok(())
}
/// Call only after retained-replay lookup; old action IDs cannot be recreated after cleanup.
pub fn fresh_action(id: Uuid, now_ms: i64) -> Result<(), Invalid> {
    if id.get_version_num() != 7 || id.get_variant() != uuid::Variant::RFC4122 {
        return Err(Invalid::Action);
    }
    let (secs, nanos) = id.get_timestamp().ok_or(Invalid::Action)?.to_unix();
    let minted = i64::try_from(u128::from(secs) * 1000 + u128::from(nanos) / 1_000_000)
        .map_err(|_| Invalid::Action)?;
    if minted > now_ms.saturating_add(60_000) || minted < now_ms.saturating_sub(300_000) {
        return Err(Invalid::Action);
    }
    Ok(())
}
/// Fail-closed own-session result without a configured/current observation.
pub fn denied(session: &str, scan: Option<&str>, reason: ContentReason) -> ContentPermission {
    ContentPermission {
        session_id: session.into(),
        scan_id: scan.map(Into::into),
        state: ContentState::Inconclusive,
        online_allowed: false,
        reason,
        files: Vec::new(),
        expires_at: None,
    }
}
/// Compare EACH selected original file against the server dictionary; no client state is trusted.
pub fn permission(
    d: &Dictionary,
    s: &ContentScan,
    created_ms: i64,
    now_ms: i64,
) -> Result<ContentPermission, Invalid> {
    validate_dictionary(d)?;
    validate_scan(s)?;
    if s.files.iter().any(|f| !d.expected.contains_key(&f.file_id)) {
        return Err(Invalid::Shape);
    }
    if d.manifest.manifest_id != s.manifest_id || d.manifest.game != s.game {
        return Ok(denied(
            &s.session_id,
            Some(&s.scan_id),
            ContentReason::PolicyChanged,
        ));
    }
    let until = created_ms.saturating_add(TTL_MS);
    if until <= now_ms {
        return Ok(denied(
            &s.session_id,
            Some(&s.scan_id),
            ContentReason::ScanExpired,
        ));
    }
    let files: Vec<ContentFileResult> = d
        .manifest
        .files
        .iter()
        .map(|expected| {
            let observed = s.files.iter().find(|f| f.file_id == expected.file_id);
            let state = match observed {
                Some(f)
                    if f.byte_length.is_some_and(|n| n != expected.byte_length)
                        || f.sha256
                            .as_ref()
                            .is_some_and(|h| Some(h) != d.expected.get(&expected.file_id)) =>
                {
                    ContentState::Incompatible
                }
                Some(f) if f.byte_length.is_some() && f.sha256.is_some() => {
                    ContentState::Compatible
                }
                _ => ContentState::Inconclusive,
            };
            ContentFileResult {
                file_id: expected.file_id.clone(),
                state,
            }
        })
        .collect();
    let state = if files.iter().any(|f| f.state == ContentState::Incompatible) {
        ContentState::Incompatible
    } else if files.iter().any(|f| f.state == ContentState::Inconclusive) {
        ContentState::Inconclusive
    } else {
        ContentState::Compatible
    };
    let reason = match state {
        ContentState::Compatible => ContentReason::Compatible,
        ContentState::Incompatible => ContentReason::IncompatibleFiles,
        ContentState::Inconclusive => ContentReason::InconclusiveScan,
    };
    Ok(ContentPermission {
        session_id: s.session_id.clone(),
        scan_id: Some(s.scan_id.clone()),
        state,
        online_allowed: state == ContentState::Compatible,
        reason,
        files,
        expires_at: fi_protocol::instant::format_unix_ms(until),
    })
}
