//! The car-state sync-id registry (CR-L53, OD-248; MATCHMAKING_API.md §5f): data, never per-mod
//! code. Each entry binds one sync id (1..=255, never reused) to one declared `ModEntry.id` (the
//! ExtraOptions `mod.json` id the launcher sends, §2.7). A seat's grant carries `sync`, the ids of
//! its declared set, and the host's announce answer `hostSync`, the ids of the world's set.

use fi_protocol::matchmaking::{ModEntry, SYNC_MAX};
use serde::Deserialize;
use std::sync::LazyLock;

/// The registry as shipped (`data/sync_registry.json`).
const REGISTRY_JSON: &str = include_str!("../data/sync_registry.json");

/// One registry row.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    sync: u8,
    #[serde(rename = "mod")]
    mod_id: String,
}

/// A validated registry: at most [`SYNC_MAX`] rows, sync ids in 1..=255 and mod ids unique.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRegistry {
    entries: Vec<Entry>,
}

/// Why a registry text is refused.
#[derive(Debug, PartialEq, Eq)]
pub struct RegistryError(pub String);

impl SyncRegistry {
    /// Parses and validates a registry text.
    pub fn parse(text: &str) -> Result<Self, RegistryError> {
        let entries: Vec<Entry> =
            serde_json::from_str(text).map_err(|e| RegistryError(e.to_string()))?;
        if entries.len() > SYNC_MAX {
            return Err(RegistryError(format!("at most {SYNC_MAX} entries")));
        }
        for (i, e) in entries.iter().enumerate() {
            if e.sync == 0 {
                return Err(RegistryError("a sync id is 1..=255".into()));
            }
            if e.mod_id.is_empty() || e.mod_id.len() > 64 {
                return Err(RegistryError("a mod id is 1 to 64 characters".into()));
            }
            if entries[..i]
                .iter()
                .any(|o| o.sync == e.sync || o.mod_id == e.mod_id)
            {
                return Err(RegistryError("sync ids and mod ids are unique".into()));
            }
        }
        Ok(Self { entries })
    }

    /// The shipped registry (validated by this crate's tests; [`crate::routes`] reads it).
    pub fn builtin() -> &'static Self {
        static BUILTIN: LazyLock<SyncRegistry> = LazyLock::new(|| {
            SyncRegistry::parse(REGISTRY_JSON).unwrap_or_else(|e| {
                // Unreachable once the tests pass; fail closed (no sync) rather than abort.
                tracing::error!(error = %e.0, "sync registry invalid: no car-state sync");
                SyncRegistry {
                    entries: Vec::new(),
                }
            })
        });
        &BUILTIN
    }

    /// The sync ids of a declared set: the registry ids whose mod id is an `id` of `mods`,
    /// ascending and unique; `None` when there are none (the claim is omitted).
    pub fn ids(&self, mods: &[ModEntry]) -> Option<Vec<u8>> {
        let mut ids: Vec<u8> = self
            .entries
            .iter()
            .filter(|e| mods.iter().any(|m| m.id == e.mod_id))
            .map(|e| e.sync)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids.truncate(SYNC_MAX);
        (!ids.is_empty()).then_some(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> ModEntry {
        ModEntry {
            id: id.into(),
            version: "1".into(),
            fingerprint: "0".repeat(64),
        }
    }

    #[test]
    fn the_shipped_registry_is_underglow_1_and_anna_2() {
        let r = SyncRegistry::parse(REGISTRY_JSON).unwrap();
        assert_eq!(&r, SyncRegistry::builtin());
        assert_eq!(r.ids(&[entry("underglow")]), Some(vec![1]));
        assert_eq!(r.ids(&[entry("fh2-anna")]), Some(vec![2]));
        assert_eq!(
            r.ids(&[entry("a-mod"), entry("fh2-anna"), entry("underglow")]),
            Some(vec![1, 2])
        );
        assert_eq!(r.ids(&[]), None);
        assert_eq!(r.ids(&[entry("other"), entry("underglow2")]), None);
    }

    #[test]
    fn a_bad_registry_is_refused() {
        for bad in [
            r#"[{"sync":0,"mod":"a"}]"#,
            r#"[{"sync":256,"mod":"a"}]"#,
            r#"[{"sync":1,"mod":""}]"#,
            r#"[{"sync":1,"mod":"a"},{"sync":1,"mod":"b"}]"#,
            r#"[{"sync":1,"mod":"a"},{"sync":2,"mod":"a"}]"#,
            r#"[{"sync":1,"mod":"a","x":1}]"#,
            r#"[{"sync":1,"mod":"a"},{"sync":2,"mod":"b"},{"sync":3,"mod":"c"},{"sync":4,"mod":"d"},{"sync":5,"mod":"e"},{"sync":6,"mod":"f"},{"sync":7,"mod":"g"},{"sync":8,"mod":"h"},{"sync":9,"mod":"i"}]"#,
            "{}",
        ] {
            assert!(SyncRegistry::parse(bad).is_err(), "{bad}");
        }
        assert!(SyncRegistry::parse("[]").is_ok());
    }
}
