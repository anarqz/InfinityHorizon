//! Environment configuration. Every service reads its settings once at startup and refuses to
//! start when one is missing or malformed: a misconfigured revision must fail its Cloud Run
//! startup probe, never serve traffic with a default it should not have.

use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::str::FromStr;

/// A setting that is missing or does not parse.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The variable is not set (or not valid UTF-8).
    #[error("environment variable {0} is required")]
    Missing(&'static str),
    /// The variable is set but does not parse.
    #[error("environment variable {name} is invalid: {reason}")]
    Invalid {
        /// Variable name.
        name: &'static str,
        /// Why it does not parse (never the value: it may be a secret).
        reason: String,
    },
}

/// A required variable.
pub fn required(name: &'static str) -> Result<String, ConfigError> {
    match std::env::var(name) {
        Ok(v) if !v.trim().is_empty() => Ok(v),
        _ => Err(ConfigError::Missing(name)),
    }
}

/// An optional variable (unset and empty are the same).
pub fn optional(name: &'static str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The keys directory of a standalone instance (`FI_KEYS_DIR`, ADR 0023 §9): one file per secret,
/// named after its variable, written once by `fi-platform keys init`.
pub const KEYS_DIR: &str = "FI_KEYS_DIR";

/// Largest key file read (a ring of a few keys is well under 1 KiB).
const KEY_FILE_MAX: u64 = 64 * 1024;

/// `<dir>/<name>`, trimmed, when it is a regular file of at most 64 KiB that is not blank. Only
/// names made of `A-Z`, `0-9` and `_` are looked up, so a name never escapes the directory.
pub fn key_file(dir: &std::path::Path, name: &str) -> Option<String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        return None;
    }
    let path = dir.join(name);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > KEY_FILE_MAX {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// A variable from `lookup` (the process environment, or a test's), else from the keys directory
/// `lookup` names in `FI_KEYS_DIR`. The environment always wins: a file never overrides a set
/// variable.
pub fn with_keys_dir(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    let set = |n: &str| lookup(n).filter(|v| !v.trim().is_empty());
    set(name).or_else(|| {
        let dir = set(KEYS_DIR)?;
        key_file(std::path::Path::new(dir.trim()), name)
    })
}

/// [`optional`], falling back to `<FI_KEYS_DIR>/<name>` (standalone instances; ADR 0023 §9).
pub fn secret(name: &str) -> Option<String> {
    with_keys_dir(&|n| std::env::var(n).ok(), name)
}

/// A variable parsed into `T`, or `default` when unset.
pub fn parse_or<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: Display,
{
    match optional(name) {
        None => Ok(default),
        Some(raw) => raw
            .trim()
            .parse()
            .map_err(|e: T::Err| ConfigError::Invalid {
                name,
                reason: e.to_string(),
            }),
    }
}

/// Who is running: logged on startup and served by `/meta`.
#[derive(Clone, Debug, Serialize)]
pub struct ServiceInfo {
    /// Cargo package name of the service (`fi-api`, `fi-platform`, `fi-cli`).
    pub service: &'static str,
    /// Cargo package version.
    pub version: &'static str,
    /// Cloud Run revision (`K_REVISION`), or `local`.
    pub revision: String,
    /// GCP project (`GCP_PROJECT_ID`); `demo-local` locally, so nothing can reach real GCP.
    pub project: String,
    /// Deployment facts a service adds (surfaces, pool), so each Cloud Run service describes itself.
    pub labels: BTreeMap<&'static str, String>,
}

impl ServiceInfo {
    /// Reads the revision and project from the environment.
    pub fn from_env(service: &'static str, version: &'static str) -> Self {
        Self {
            service,
            version,
            revision: optional("K_REVISION").unwrap_or_else(|| "local".to_owned()),
            project: optional("GCP_PROJECT_ID").unwrap_or_else(|| "demo-local".to_owned()),
            labels: BTreeMap::new(),
        }
    }

    /// Adds a label.
    pub fn with_label(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.labels.insert(key, value.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_variables_fall_back_or_fail() {
        // A name no environment sets.
        const NAME: &str = "FI_RUNTIME_TEST_SURELY_UNSET_7F3A";
        assert!(matches!(required(NAME), Err(ConfigError::Missing(NAME))));
        assert_eq!(optional(NAME), None);
        assert_eq!(parse_or::<u16>(NAME, 8080).unwrap(), 8080);
    }

    #[test]
    fn the_keys_directory_backs_unset_variables_only() {
        let dir = std::env::temp_dir().join(format!("fi-runtime-keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("FI_SECRET_A"), "from-file\n").unwrap();
        std::fs::write(dir.join("FI_BLANK"), "  \n").unwrap();
        let dir_text = dir.to_string_lossy().into_owned();
        let env = |set: bool| {
            let dir_text = dir_text.clone();
            move |n: &str| match n {
                KEYS_DIR => Some(dir_text.clone()),
                "FI_SECRET_A" if set => Some("from-env".to_owned()),
                _ => None,
            }
        };
        assert_eq!(
            with_keys_dir(&env(false), "FI_SECRET_A").as_deref(),
            Some("from-file")
        );
        assert_eq!(
            with_keys_dir(&env(true), "FI_SECRET_A").as_deref(),
            Some("from-env")
        );
        assert_eq!(with_keys_dir(&env(false), "FI_BLANK"), None);
        assert_eq!(with_keys_dir(&env(false), "FI_MISSING"), None);
        // A name that is not a variable name never reaches the file system.
        assert_eq!(key_file(&dir, "../FI_SECRET_A"), None);
        assert_eq!(key_file(&dir, "fi_secret_a"), None);
        // Without FI_KEYS_DIR nothing is read.
        assert_eq!(with_keys_dir(&|_| None, "FI_SECRET_A"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
