//! Signed complete Infinity runtime descriptor, not original-game content (LAUNCHER_UPDATE.md).
//! These are wire shapes only. Consumers verify the raw-byte signature and semantic bounds.
use serde::{Deserialize, Serialize};

/// Detached descriptor-signature domain, including its one LF and no trailing digest LF.
pub const RELEASE_SIGNATURE_DOMAIN: &str = "fi-launcher-release-v1\n";
/// Maximum raw descriptor length.
pub const MAX_DESCRIPTOR_BYTES: usize = 65_536;
/// Maximum regular managed runtime files.
pub const MAX_RELEASE_FILES: usize = 128;
/// Maximum downloaded ZIP length (512MiB).
pub const MAX_PACKAGE_BYTES: u64 = 536_870_912;
/// Maximum total expanded bytes (1GiB).
pub const MAX_UNPACKED_BYTES: u64 = 1_073_741_824;

/// A raw-byte signed stable Windows x64 runtime release descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LauncherReleaseDescriptor {
    /// Must equal one.
    pub schema_version: u8,
    /// Must equal infinity-horizon.
    pub product: String,
    /// Must equal anarqz/InfinityHorizon.
    pub repository: String,
    /// Exact v-prefixed full version; matches upstream release tag.
    pub tag: String,
    /// Must equal stable.
    pub channel: String,
    /// Must equal windows-x64.
    pub platform: String,
    /// Strict bounded SemVer; no prerelease in stable descriptors.
    pub version: String,
    /// Positive JSON-safe monotonic release sequence.
    pub release_sequence: u64,
    /// Lowercase forty-hex source commit.
    pub source_commit: String,
    /// UTC6, at most 300 seconds in the future.
    pub issued_at: String,
    /// UTC6, after issuance and at most 180 days later.
    pub expires_at: String,
    /// Exact named ZIP with bounded size and digest.
    pub package: LauncherReleasePackage,
    /// Complete managed runtime inventory, never game/user files.
    pub files: Vec<LauncherReleaseFile>,
}

/// Exact package asset bound by the descriptor signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LauncherReleasePackage {
    /// infinity-horizon-{version}-windows-x64.zip, no URL or path.
    pub name: String,
    /// Exact ZIP length, positive and at most 512MiB.
    pub byte_length: u64,
    /// Lowercase sixty-four-hex SHA-256 of complete ZIP bytes.
    pub sha256: String,
}

/// One regular file relative to the managed Infinity runtime installation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LauncherReleaseFile {
    /// Bounded safe ASCII relative slash-separated path, unique case-insensitively.
    pub path: String,
    /// Exact expanded length, total inventory at most 1GiB.
    pub byte_length: u64,
    /// Lowercase sixty-four-hex SHA-256 of file bytes.
    pub sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn sample() -> Value {
        // Synthetic shape only: not an available or signed release/package.
        json!({
            "schemaVersion": 1, "product": "infinity-horizon",
            "repository": "anarqz/InfinityHorizon", "tag": "v0.2.0",
            "channel": "stable", "platform": "windows-x64", "version": "0.2.0",
            "releaseSequence": 1, "sourceCommit": "0".repeat(40),
            "issuedAt": "2026-10-20T12:00:00.000000Z",
            "expiresAt": "2026-10-21T12:00:00.000000Z",
            "package": {"name": "infinity-horizon-0.2.0-windows-x64.zip",
                "byteLength": 100, "sha256": "0".repeat(64)},
            "files": [{"path": "infinity-updater.exe", "byteLength": 1,
                "sha256": "0".repeat(64)}]
        })
    }

    #[test]
    fn exact_camel_case_shape_round_trips() {
        let value = sample();
        let dto: LauncherReleaseDescriptor = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(dto).unwrap(), value);
        assert_eq!(
            RELEASE_SIGNATURE_DOMAIN.as_bytes(),
            b"fi-launcher-release-v1\n"
        );
    }

    #[test]
    fn signed_schema_rejects_unknown_missing_duplicate_and_noninteger_fields() {
        for pointer in ["", "/package", "/files/0"] {
            let mut value = sample();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("untrustedUrl".into(), json!("https://invalid.test"));
            assert!(serde_json::from_value::<LauncherReleaseDescriptor>(value).is_err());
        }
        for key in sample().as_object().unwrap().keys() {
            let mut value = sample();
            value.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<LauncherReleaseDescriptor>(value).is_err(),
                "{key}"
            );
        }
        for bad in [json!(-1), json!(1.5), Value::Null, json!("1")] {
            let mut value = sample();
            value["releaseSequence"] = bad;
            assert!(serde_json::from_value::<LauncherReleaseDescriptor>(value).is_err());
        }
        let raw = serde_json::to_string(&sample()).unwrap();
        let duplicate = raw.replacen('{', "{\"schemaVersion\":1,", 1);
        assert!(serde_json::from_str::<LauncherReleaseDescriptor>(&duplicate).is_err());
    }
}
