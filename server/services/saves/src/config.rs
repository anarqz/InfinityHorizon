//! Settings of the saves deployable, read once at startup. The deployment verifies and never
//! mints: `FI_TOKEN_VERIFY_KEYS` holds public keys, and a configured `FI_TOKEN_SIGNING_KEYS`
//! (the gate's private ring) refuses the start, so a mis-wired secret cannot reach this service.
//!
//! | Variable | Default | What |
//! | --- | --- | --- |
//! | `FI_TOKEN_VERIFY_KEYS` | required | `kid:base64(public key)` of the game access ticket (the gate's ring) |
//! | `FI_PLATFORM_VERIFY_KEYS` | `FI_TOKEN_VERIFY_KEYS` locally; required in prod | public keys of core's launcher access tokens (the save line) |
//! | `FI_EDGE_SECRET` | unset (no origin lock); required in prod | the origin lock (ADR 0903 §1) |
//! | `FI_GAMES` | `fh2` | games served (ADR 0902) |
//! | `FI_INSTANCE_ID` | hostname | `X-Fi-Instance`, the liveness rows' instance |
//! | `FI_GAME_LIVENESS_CACHE_MS` | 60000 | per-instance liveness cache, 0 to 60000 |
//! | `FI_MIN_HOOK` | unset (any hook) | the oldest hook served on the proxied game requests (`0.3.0+fh2online.60`; GAME_PLATFORM.md §3a) |
//! | `FI_GUARD_ENFORCE` | `false` (shadow) | the Hardcore guardrails refuse a score of 60 or more (OD-62, CLOUD_SAVES.md §7.3); ceilings refuse either way |
//! | `FI_UNIVERSE_CACHE_MS` | 5000 | the `nearby` read cache's TTL (0 disables) |
//! | `FI_EDGE_CACHE_BOARDS` | `false` | no effect since the `/saves/v1` leaderboards were retired (2026-10-27); still checked (`true` or `false`) so an existing environment starts |
//! | `FI_BLOB_BACKEND` | `firebase` | `firebase`: the cloud-saves bucket; `fs`: `FI_BLOB_DIR` (refused in prod; OD-122) |
//! | `FI_BLOB_DIR` | `/data/blobs` | the `fs` backend's directory |
//! | `CLOUD_SAVES_BUCKET` | required with `firebase` | the cloud-saves bucket (save blobs, the game's ghosts) |
//! | `STORAGE_EMULATOR_HOST` | unset: Cloud Storage with the metadata server's credentials | the emulator (refused in prod) |

use fi_auth::relay_grant::PublicKeys;
use fi_runtime::config::ConfigError;
use fi_runtime::http::EdgeSecret;
use fi_surfaces::config::{Env, is_prod};
use std::sync::Arc;
use std::time::Duration;

/// TTLs of the read caches (ADR 0016 §2): `nearby` only since the boards, rivals and ghosts of
/// `/saves/v1` were retired (2026-10-27).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheTtls {
    /// Nearby (5 s).
    pub nearby: Duration,
}

impl Default for CacheTtls {
    fn default() -> Self {
        Self {
            nearby: Duration::from_secs(5),
        }
    }
}

/// The deployment's settings.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Validated global launcher compatibility floor.
    pub min_launcher: String,
    /// Required content policy; explicit local synthetic-test bypass only.
    pub content_required: bool,
    /// Public keys of the game access ticket.
    pub verify_keys: PublicKeys,
    /// Public keys of the launcher's access tokens.
    pub access_keys: PublicKeys,
    /// The origin lock.
    pub edge: Option<Arc<EdgeSecret>>,
    /// Games served.
    pub games: Vec<String>,
    /// This instance.
    pub instance_id: String,
    /// Liveness cache TTL (ms).
    pub liveness_ms: u32,
    /// Read cache TTLs.
    pub ttls: CacheTtls,
    /// `FI_EDGE_CACHE_BOARDS`: no effect since the `/saves/v1` leaderboards were retired.
    pub edge_boards: bool,
    /// Storage emulator endpoint (`None`: Cloud Storage as the service account).
    pub storage_endpoint: Option<String>,
    /// Bucket (empty with the `fs` backend).
    pub bucket: String,
    /// The `fs` backend (`FI_BLOB_BACKEND=fs`); `None`: the bucket.
    pub fs_blobs: Option<fi_storage::Blobs>,
    /// `FI_MIN_HOOK`: the oldest hook the proxied game requests accept (`None`: any).
    pub min_hook: Option<String>,
    /// `FI_LAUNCHER_ACCOUNT_SCALE`: the per-account buckets of the save line's launcher routes
    /// (ACCOUNTS.md §9, OD-215).
    pub accounts: Arc<fi_surfaces::config::AccountLimiter>,
    /// `FI_GUARD_ENFORCE`: the Hardcore guardrails refuse a score of 60 or more (default off: shadow;
    /// OD-62). Ceilings refuse either way.
    pub guard_enforce: bool,
    /// CR-S51: `FI_CLOUD_SAVES`, `FI_HARDCORE_START`, `FI_FRESH_START_MAX_*` (unset: production's
    /// OD-52 lines, the seed while one is active, ceilings 0).
    pub cloud_saves: fi_surfaces::config::CloudSavesSettings,
}

fn invalid(name: &'static str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.to_owned(),
    }
}

fn required(env: &dyn Env, name: &'static str) -> Result<String, ConfigError> {
    env.get(name).ok_or(ConfigError::Missing(name))
}

impl Settings {
    /// Where the blobs live (a `/meta` label): `fs`, `emulator` or `cloud`.
    pub fn storage_label(&self) -> &'static str {
        match (&self.fs_blobs, &self.storage_endpoint) {
            (Some(_), _) => "fs",
            (None, Some(_)) => "emulator",
            (None, None) => "cloud",
        }
    }

    /// Reads and checks the settings.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        if env.get("FI_TOKEN_SIGNING_KEYS").is_some() {
            return Err(invalid(
                "FI_TOKEN_SIGNING_KEYS",
                "the saves service verifies tickets and never mints them: configure FI_TOKEN_VERIFY_KEYS (public keys) only",
            ));
        }
        let verify_keys = PublicKeys::parse(
            "FI_TOKEN_VERIFY_KEYS",
            &required(env, "FI_TOKEN_VERIFY_KEYS")?,
        )
        .map_err(|e| invalid("FI_TOKEN_VERIFY_KEYS", e.reason))?;
        let prod = is_prod(env);
        if env.get("FI_PLATFORM_SIGNING_KEYS").is_some() {
            return Err(invalid(
                "FI_PLATFORM_SIGNING_KEYS",
                "the saves service verifies access tokens and never signs them: configure FI_PLATFORM_VERIFY_KEYS (public keys) only",
            ));
        }
        let access_keys = match env.get("FI_PLATFORM_VERIFY_KEYS") {
            Some(text) => PublicKeys::parse("FI_PLATFORM_VERIFY_KEYS", &text)
                .map_err(|e| invalid("FI_PLATFORM_VERIFY_KEYS", e.reason))?,
            None if prod => return Err(ConfigError::Missing("FI_PLATFORM_VERIFY_KEYS")),
            // Locally one ring signs access tokens and tickets (compose).
            None => verify_keys.clone(),
        };
        let edge = fi_surfaces::config::edge_secret(env)?;
        let games: Vec<String> = env
            .get("FI_GAMES")
            .unwrap_or_else(|| "fh2".into())
            .split(',')
            .map(|g| g.trim().to_owned())
            .filter(|g| !g.is_empty())
            .collect();
        if games.is_empty()
            || games.iter().any(|g| {
                !(2..=16).contains(&g.len())
                    || !g.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                    || !g
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
            })
        {
            return Err(invalid("FI_GAMES", "game codes, comma separated"));
        }
        let number = |name: &'static str, default: u32| -> Result<u32, ConfigError> {
            match env.get(name) {
                None => Ok(default),
                Some(v) => v.trim().parse().map_err(|_| invalid(name, "not a number")),
            }
        };
        let liveness_ms = number(
            "FI_GAME_LIVENESS_CACHE_MS",
            fi_surfaces::game::cache::MAX_TTL_MS,
        )?;
        if liveness_ms > fi_surfaces::game::cache::MAX_TTL_MS {
            return Err(invalid("FI_GAME_LIVENESS_CACHE_MS", "0 to 60000"));
        }
        let ttls = match env.get("FI_UNIVERSE_CACHE_MS") {
            None => CacheTtls::default(),
            Some(v) => {
                let ms: u64 = v
                    .trim()
                    .parse()
                    .ok()
                    .filter(|ms| *ms <= 60_000)
                    .ok_or_else(|| invalid("FI_UNIVERSE_CACHE_MS", "0 to 60000"))?;
                CacheTtls {
                    nearby: Duration::from_millis(ms),
                }
            }
        };
        let edge_boards = match env.get("FI_EDGE_CACHE_BOARDS").as_deref().map(str::trim) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid("FI_EDGE_CACHE_BOARDS", "true or false")),
        };
        let storage_endpoint = env.get("STORAGE_EMULATOR_HOST");
        if prod && storage_endpoint.is_some() {
            return Err(invalid(
                "STORAGE_EMULATOR_HOST",
                "the Storage emulator is refused when APP_ENV or FI_ENV is prod",
            ));
        }
        let fs_blobs = fi_storage::Blobs::from_parts(
            env.get("FI_BLOB_BACKEND").as_deref(),
            env.get("FI_BLOB_DIR").as_deref(),
            prod,
        )
        .map_err(|e| invalid("FI_BLOB_BACKEND", &e.to_string()))?;
        let bucket = match fs_blobs {
            Some(_) => env.get("CLOUD_SAVES_BUCKET").unwrap_or_default(),
            None => required(env, "CLOUD_SAVES_BUCKET")?,
        };
        let instance_id = env
            .get("FI_INSTANCE_ID")
            .or_else(|| env.get("HOSTNAME"))
            .unwrap_or_else(|| "local".into());
        if !(1..=128).contains(&instance_id.len())
            || !instance_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err(invalid(
                "FI_INSTANCE_ID",
                "1 to 128 letters, digits, dashes, dots or underscores",
            ));
        }
        Ok(Self {
            min_launcher: fi_surfaces::config::launcher_versions_from_env(env)?.0,
            content_required: fi_surfaces::config::content_required_from_env(env)?,
            verify_keys,
            access_keys,
            edge,
            games,
            instance_id,
            liveness_ms,
            ttls,
            edge_boards,
            storage_endpoint: storage_endpoint.map(|e| e.trim().to_owned()),
            bucket: bucket.trim().to_owned(),
            fs_blobs,
            min_hook: fi_surfaces::game::proxy::min_hook_from_env(env)?,
            guard_enforce: fi_surfaces::config::guard_enforce_from_env(env)?,
            cloud_saves: fi_surfaces::config::CloudSavesSettings::from_env(env)?,
            accounts: Arc::new(fi_surfaces::config::AccountLimiter::from_env(env)?),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: &str = LOCAL_VERIFY_KEYS;

    fn env(extra: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        let mut v: Vec<(&'static str, String)> = vec![
            ("FI_TOKEN_VERIFY_KEYS", KEYS.into()),
            ("STORAGE_EMULATOR_HOST", "http://fake-gcs:4453".into()),
            ("CLOUD_SAVES_BUCKET", "fi-dev-cloud-saves".into()),
        ];
        for (k, val) in extra {
            v.retain(|(n, _)| n != k);
            v.push((k, (*val).to_owned()));
        }
        v
    }

    /// The public half of the well-known local signing key of `.env.example`, set as
    /// `FI_TOKEN_VERIFY_KEYS` by the local compose files (ADR 0023).
    pub const LOCAL_VERIFY_KEYS: &str = "at-local-1:NRMFDrDXMyz5ESomMUulIiSVrVUYWQ+IPBFTk4zYw30=";

    #[test]
    fn the_local_verify_keys_are_the_local_ring_public_half() {
        let ring = fi_auth::keys::SigningKeys::parse(
            "FI_TOKEN_SIGNING_KEYS",
            "at-local-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=",
        )
        .unwrap();
        assert_eq!(PublicKeys::of(&ring).to_text(), LOCAL_VERIFY_KEYS);
    }

    #[test]
    fn the_fs_blob_backend_needs_no_bucket_and_is_refused_in_prod() {
        let mut e = env(&[("FI_BLOB_BACKEND", "fs"), ("FI_BLOB_DIR", "/srv/blobs")]);
        e.retain(|(n, _)| *n != "CLOUD_SAVES_BUCKET" && *n != "STORAGE_EMULATOR_HOST");
        let s = Settings::from_env(&e).unwrap();
        assert_eq!(s.storage_label(), "fs");
        assert!(s.bucket.is_empty());
        let mut prod = e.clone();
        prod.push(("FI_ENV", "prod".into()));
        prod.push(("FI_EDGE_SECRET", "1:".to_owned() + &"a".repeat(43)));
        prod.push(("FI_PLATFORM_VERIFY_KEYS", LOCAL_VERIFY_KEYS.into()));
        match Settings::from_env(&prod) {
            Err(ConfigError::Invalid { name, .. }) => assert_eq!(name, "FI_BLOB_BACKEND"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            Settings::from_env(&env(&[])).unwrap().storage_label(),
            "emulator"
        );
    }

    #[test]
    fn verify_only_and_the_defaults() {
        let s = Settings::from_env(&env(&[])).unwrap();
        assert_eq!(s.games, vec!["fh2".to_owned()]);
        assert_eq!(s.ttls, CacheTtls::default());
        assert!(!s.edge_boards);
        let e = Settings::from_env(&env(&[(
            "FI_TOKEN_SIGNING_KEYS",
            "at-local-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=",
        )]))
        .unwrap_err();
        assert!(e.to_string().contains("never mints"), "{e}");
        assert!(Settings::from_env(&env(&[("FI_GAMES", "FH2")])).is_err());
        assert!(Settings::from_env(&env(&[("FI_UNIVERSE_CACHE_MS", "70000")])).is_err());
        assert!(Settings::from_env(&env(&[("APP_ENV", "prod")])).is_err());
        assert_eq!(
            s.access_keys.to_text(),
            s.verify_keys.to_text(),
            "one ring locally"
        );
        // Prod: no emulator, Cloud Storage with the metadata server's credentials, the
        // platform's public keys and the edge lock required, core's private ring refused.
        let mut prod = env(&[("FI_ENV", "prod")]);
        prod.retain(|(k, _)| *k != "STORAGE_EMULATOR_HOST");
        let missing = |e: &Vec<(&'static str, String)>| match Settings::from_env(e) {
            Err(ConfigError::Missing(n)) => n,
            other => panic!("{other:?}"),
        };
        assert_eq!(missing(&prod), "FI_PLATFORM_VERIFY_KEYS");
        prod.push((
            "FI_PLATFORM_VERIFY_KEYS",
            "pt-1:NRMFDrDXMyz5ESomMUulIiSVrVUYWQ+IPBFTk4zYw30=".into(),
        ));
        assert_eq!(missing(&prod), "FI_EDGE_SECRET");
        prod.push(("FI_EDGE_SECRET", "e".repeat(32)));
        let p = Settings::from_env(&prod).unwrap();
        assert!(p.storage_endpoint.is_none() && p.edge.is_some());
        let mut signing = prod.clone();
        signing.push(("FI_PLATFORM_SIGNING_KEYS", "x".into()));
        assert!(Settings::from_env(&signing).is_err());
        assert_eq!(s.min_hook, None);
        assert!(!s.guard_enforce, "shadow by default");
        assert!(
            Settings::from_env(&env(&[("FI_GUARD_ENFORCE", "true")]))
                .unwrap()
                .guard_enforce
        );
        assert!(Settings::from_env(&env(&[("FI_GUARD_ENFORCE", "on")])).is_err());
        assert!(Settings::from_env(&env(&[("FI_MIN_HOOK", "latest")])).is_err());
        let hooked = Settings::from_env(&env(&[("FI_MIN_HOOK", " 0.3.0+fh2online.60 ")])).unwrap();
        assert_eq!(hooked.min_hook.as_deref(), Some("0.3.0+fh2online.60"));
        let one = Settings::from_env(&env(&[("FI_UNIVERSE_CACHE_MS", "50")])).unwrap();
        assert_eq!(one.ttls.nearby, Duration::from_millis(50));
    }
}
