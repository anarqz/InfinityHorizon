//! fi-core's own settings (read once at startup; a revision that could reach prod with local
//! settings refuses to start):
//!
//! | Variable | Default (local) | Prod |
//! | --- | --- | --- |
//! | `FI_WEB_ORIGIN` | `http://127.0.0.1:3120` | required, `https://` |
//! | `FI_TURNSTILE_MODE` | `test` | `live` only |
//! | `FI_TURNSTILE_SECRET` | the always-pass test secret | required, never a test secret |
//! | `FI_TURNSTILE_VERIFY_URL` | Cloudflare's siteverify (live mode only) | `https://` |
//! | `FI_EDGE_SECRET` | unset (no origin lock) | required |
//! | `FI_MCP_ENABLED` | `false` | |
//! | `FI_MCP_SIGNING_KEYS` | required when MCP is enabled | no well-known local key |
//! | `FI_PROBE_{GATE,SAVES,MATCHMAKING,RELAY}_URL` | unset (`unknown` in status) | |
//! | `FI_SUPPORT_{KOFI,HELP,REPORT}_URL` | placeholders | |
//! | `FI_FEED_KEYS` | unset (the Discord feed answers 503) | SHA-256 digests of the bot keys (CR-S63) |
//! | `FI_DISCORD_LINK_URL` | unset (linking answers 503) | the bot's `https://` origin (CR-S63) |
//!
//! Turnstile test mode never calls Cloudflare: it answers as Cloudflare documents its test
//! secrets do (`1x…AA` passes, `2x…AA` fails, `3x…AA` reports a spent token), so local runs and
//! tests make no network call to Cloudflare (AGENTS.md rule 4). Live mode posts to siteverify.

use crate::turnstile;
use fi_auth::keys::SigningKeys;
use fi_protocol::core::SupportLinks;
use fi_runtime::config::ConfigError;
use fi_runtime::http::EdgeSecret;
use fi_runtime::ratelimit::Rate;
use fi_surfaces::config::{Env, is_prod};
use std::sync::Arc;

fn invalid(name: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

/// `scheme://host[:port]` without a path; `https` only when `secure`.
fn origin(name: &'static str, value: &str, secure: bool) -> Result<String, ConfigError> {
    let v = value.trim().trim_end_matches('/');
    let rest = match (v.strip_prefix("https://"), v.strip_prefix("http://")) {
        (Some(r), _) => r,
        (None, Some(r)) if !secure => r,
        _ => {
            return Err(invalid(
                name,
                if secure {
                    "an https:// origin"
                } else {
                    "an http(s):// origin"
                },
            ));
        }
    };
    if rest.is_empty()
        || rest.contains(['/', '?', '#', '@'])
        || !rest.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(invalid(name, "an origin without a path"));
    }
    Ok(v.to_owned())
}

/// How Turnstile tokens are verified.
#[derive(Clone)]
pub enum TurnstileConfig {
    /// Deferred web: challenged routes remain unavailable, never passed.
    Deferred,
    /// Cloudflare's documented behaviour of a test secret, without a network call.
    Test(turnstile::TestSecret),
    /// Cloudflare's siteverify (or a stand-in URL in tests).
    Live {
        /// The widget's secret.
        secret: String,
        /// The siteverify URL.
        url: String,
        /// The hostname the widget must have been solved on (the web's).
        hostname: Option<String>,
    },
}

impl std::fmt::Debug for TurnstileConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnstileConfig::Deferred => f.write_str("Deferred"),
            TurnstileConfig::Test(t) => write!(f, "Test({t:?})"),
            TurnstileConfig::Live { url, hostname, .. } => {
                write!(
                    f,
                    "Live {{ url: {url:?}, hostname: {hostname:?}, secret: *** }}"
                )
            }
        }
    }
}

/// The MCP skeleton's switch and keys (ADR 0018 §5).
#[derive(Clone, Debug)]
pub struct McpConfig {
    /// `FI_MCP_ENABLED=true`.
    pub enabled: bool,
    /// `FI_MCP_SIGNING_KEYS` (verifies MCP access tokens; required when enabled).
    pub keys: Option<SigningKeys>,
}

/// Readiness URLs probed for `GET /core/v1/status` (each `…/health/ready`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProbeTargets {
    /// fi-gate.
    pub gate: Option<String>,
    /// fi-saves.
    pub saves: Option<String>,
    /// fi-matchmaking.
    pub matchmaking: Option<String>,
    /// The relay VM's health sidecar.
    pub relay: Option<String>,
}

/// The in-app token buckets (ADR 0903 §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rates {
    /// Public reads per client network (60/min).
    pub public_read: Rate,
    /// Public writes per client network (10/min).
    pub public_write: Rate,
    /// Writes per account (30/min).
    pub account_write: Rate,
    /// Comments per account (10/min).
    pub comment: Rate,
    /// Friend-list and invite-inbox reads per account (30/min; the overlay polls invites every
    /// 10 s): CORE_API.md §4.1, §4.2.
    pub account_read: Rate,
    /// The Discord bot's routes per client network (60/min; CORE_API.md §5a.1).
    pub bot: Rate,
}

impl Default for Rates {
    fn default() -> Self {
        Self {
            public_read: Rate::per_minute(60),
            public_write: Rate::per_minute(10),
            account_write: Rate::per_minute(30),
            comment: Rate::per_minute(10),
            account_read: Rate::per_minute(30),
            bot: Rate::per_minute(60),
        }
    }
}

/// Everything fi-core adds to fi-api's identity configuration.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// Explicit web deployment scope; false disables portal/challenged routes and cookies.
    pub web_enabled: bool,
    /// The web's origin: the only CORS origin, the only `Origin` a cookie write may carry.
    pub web_origin: String,
    /// The public origin of core (MCP resource and issuer; `FI_PUBLIC_CORE_URL`).
    pub public_origin: String,
    /// Turnstile.
    pub turnstile: TurnstileConfig,
    /// The origin lock.
    pub edge: Option<Arc<EdgeSecret>>,
    /// MCP.
    pub mcp: McpConfig,
    /// Status probes.
    pub probes: ProbeTargets,
    /// `GET /core/v1/me/support` links.
    pub support: SupportLinks,
    /// Token buckets.
    pub rates: Rates,
    /// `FI_FEED_KEYS`: the bot keys' digests (CR-S63); `None`: the feed is off.
    pub feed_keys: Option<crate::discord::FeedKeys>,
    /// `FI_DISCORD_LINK_URL`: the bot's origin for link pages (CR-S63); `None`: linking is off.
    pub discord_link_url: Option<String>,
}

impl CoreConfig {
    /// Reads the variables of the module table.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        let prod = is_prod(env);
        let web_origin = match env.get("FI_WEB_ORIGIN") {
            Some(v) => origin("FI_WEB_ORIGIN", &v, prod)?,
            None if prod => return Err(ConfigError::Missing("FI_WEB_ORIGIN")),
            None => "http://127.0.0.1:3120".to_owned(),
        };
        let public_origin = match env.get("FI_PUBLIC_CORE_URL") {
            Some(v) => origin("FI_PUBLIC_CORE_URL", &v, prod)?,
            None if prod => return Err(ConfigError::Missing("FI_PUBLIC_CORE_URL")),
            None => "http://127.0.0.1:8480".to_owned(),
        };
        let web_enabled = match env.get("FI_WEB_ENABLED").as_deref() {
            None | Some("true") => true,
            Some("false") => false,
            Some(_) => return Err(invalid("FI_WEB_ENABLED", "true or false")),
        };
        let turnstile = if !web_enabled {
            TurnstileConfig::Deferred
        } else {
            match env.get("FI_TURNSTILE_MODE").as_deref().map(str::trim) {
                None | Some("test") => {
                    if prod {
                        return Err(invalid(
                            "FI_TURNSTILE_MODE",
                            "test mode is refused when APP_ENV or FI_ENV is prod",
                        ));
                    }
                    let secret = env
                        .get("FI_TURNSTILE_SECRET")
                        .unwrap_or_else(|| turnstile::TEST_SECRET_PASS.to_owned());
                    TurnstileConfig::Test(turnstile::TestSecret::parse(secret.trim()).ok_or_else(
                        || {
                            invalid(
                                "FI_TURNSTILE_SECRET",
                                "test mode takes one of Cloudflare's documented test secrets",
                            )
                        },
                    )?)
                }
                Some("live") => {
                    let secret = env
                        .get("FI_TURNSTILE_SECRET")
                        .ok_or(ConfigError::Missing("FI_TURNSTILE_SECRET"))?
                        .trim()
                        .to_owned();
                    if turnstile::TestSecret::parse(&secret).is_some() {
                        return Err(invalid(
                            "FI_TURNSTILE_SECRET",
                            "a test secret is refused in live mode",
                        ));
                    }
                    if secret.len() < 16 || !secret.bytes().all(|b| b.is_ascii_graphic()) {
                        return Err(invalid("FI_TURNSTILE_SECRET", "not a secret"));
                    }
                    let url = env
                        .get("FI_TURNSTILE_VERIFY_URL")
                        .unwrap_or_else(|| turnstile::SITEVERIFY_URL.to_owned());
                    let url = url.trim().to_owned();
                    if !(url.starts_with("https://") || (!prod && url.starts_with("http://"))) {
                        return Err(invalid(
                            "FI_TURNSTILE_VERIFY_URL",
                            if prod { "https only" } else { "an http(s) URL" },
                        ));
                    }
                    let hostname = web_origin
                        .split_once("://")
                        .map(|(_, h)| h.split(':').next().unwrap_or(h).to_owned());
                    TurnstileConfig::Live {
                        secret,
                        url,
                        hostname,
                    }
                }
                Some(_) => return Err(invalid("FI_TURNSTILE_MODE", "test or live")),
            }
        };
        let edge = fi_surfaces::config::edge_secret(env)?;
        let enabled = match env.get("FI_MCP_ENABLED").as_deref().map(str::trim) {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid("FI_MCP_ENABLED", "true or false")),
        };
        let keys = match env.get("FI_MCP_SIGNING_KEYS") {
            Some(text) => {
                let ring = SigningKeys::parse("FI_MCP_SIGNING_KEYS", &text)
                    .map_err(|e| invalid("FI_MCP_SIGNING_KEYS", e.reason))?;
                if prod && ring.has_local_dev_key() {
                    return Err(invalid(
                        "FI_MCP_SIGNING_KEYS",
                        "a well-known local development key is refused in prod",
                    ));
                }
                Some(ring)
            }
            None if enabled => return Err(ConfigError::Missing("FI_MCP_SIGNING_KEYS")),
            None => None,
        };
        let probe = |name: &'static str| -> Result<Option<String>, ConfigError> {
            env.get(name)
                .map(|v| {
                    let v = v.trim().to_owned();
                    if v.starts_with("http://") || v.starts_with("https://") {
                        Ok(v)
                    } else {
                        Err(invalid(name, "an http(s) URL"))
                    }
                })
                .transpose()
        };
        let feed_keys = env
            .get("FI_FEED_KEYS")
            .filter(|v| !v.trim().is_empty())
            .map(|v| crate::discord::FeedKeys::parse(&v).map_err(|r| invalid("FI_FEED_KEYS", r)))
            .transpose()?;
        let discord_link_url = env
            .get("FI_DISCORD_LINK_URL")
            .filter(|v| !v.trim().is_empty())
            .map(|v| origin("FI_DISCORD_LINK_URL", &v, prod))
            .transpose()?;
        let link =
            |name: &'static str, default: &str| env.get(name).unwrap_or_else(|| default.to_owned());
        Ok(Self {
            web_enabled,
            web_origin: web_origin.clone(),
            public_origin,
            turnstile,
            edge,
            mcp: McpConfig { enabled, keys },
            probes: ProbeTargets {
                gate: probe("FI_PROBE_GATE_URL")?,
                saves: probe("FI_PROBE_SAVES_URL")?,
                matchmaking: probe("FI_PROBE_MATCHMAKING_URL")?,
                relay: probe("FI_PROBE_RELAY_URL")?,
            },
            support: SupportLinks {
                kofi: link("FI_SUPPORT_KOFI_URL", "https://ko-fi.com/"),
                help: link("FI_SUPPORT_HELP_URL", &format!("{web_origin}/help")),
                report: link(
                    "FI_SUPPORT_REPORT_URL",
                    &format!("{web_origin}/help/report"),
                ),
            },
            rates: Rates::default(),
            feed_keys,
            discord_link_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn local_defaults_use_test_keys_and_no_lock() {
        let c = CoreConfig::from_env(&env(&[])).unwrap();
        assert_eq!(c.web_origin, "http://127.0.0.1:3120");
        assert!(matches!(
            c.turnstile,
            TurnstileConfig::Test(turnstile::TestSecret::Pass)
        ));
        assert!(c.edge.is_none() && !c.mcp.enabled && c.mcp.keys.is_none());
        assert_eq!(c.probes, ProbeTargets::default());
        assert!(c.feed_keys.is_none() && c.discord_link_url.is_none());
        let digest = "ab".repeat(32);
        let d = CoreConfig::from_env(&env(&[
            ("FI_FEED_KEYS", digest.as_str()),
            ("FI_DISCORD_LINK_URL", "http://127.0.0.1:8485/"),
        ]))
        .unwrap();
        assert!(d.feed_keys.is_some());
        assert_eq!(d.discord_link_url.as_deref(), Some("http://127.0.0.1:8485"));
        assert!(CoreConfig::from_env(&env(&[("FI_FEED_KEYS", "not-a-digest")])).is_err());
        assert!(CoreConfig::from_env(&env(&[("FI_DISCORD_LINK_URL", "ftp://x")])).is_err());
        let fail = CoreConfig::from_env(&env(&[(
            "FI_TURNSTILE_SECRET",
            turnstile::TEST_SECRET_FAIL,
        )]))
        .unwrap();
        assert!(matches!(
            fail.turnstile,
            TurnstileConfig::Test(turnstile::TestSecret::Fail)
        ));
        assert!(
            CoreConfig::from_env(&env(&[("FI_TURNSTILE_SECRET", "some-other-secret-value")]))
                .is_err(),
            "test mode takes test secrets only"
        );
        assert!(CoreConfig::from_env(&env(&[("FI_MCP_ENABLED", "true")])).is_err());
        assert!(CoreConfig::from_env(&env(&[("FI_MCP_ENABLED", "yes")])).is_err());
        assert!(CoreConfig::from_env(&env(&[("FI_WEB_ORIGIN", "http://x/app")])).is_err());
    }

    #[test]
    fn prod_refuses_local_settings() {
        let edge = "e".repeat(32);
        let base = [
            ("APP_ENV", "prod"),
            ("FI_WEB_ORIGIN", "https://app.example.test"),
            ("FI_PUBLIC_CORE_URL", "https://api.example.test"),
            ("FI_TURNSTILE_MODE", "live"),
            ("FI_TURNSTILE_SECRET", "placeholder-live-secret-not-real"),
            ("FI_EDGE_SECRET", edge.as_str()),
        ];
        let ok = CoreConfig::from_env(&env(&base)).unwrap();
        match &ok.turnstile {
            TurnstileConfig::Live { url, hostname, .. } => {
                assert_eq!(url, turnstile::SITEVERIFY_URL);
                assert_eq!(hostname.as_deref(), Some("app.example.test"));
            }
            other => panic!("{other:?}"),
        }
        assert!(!format!("{:?}", ok.turnstile).contains("placeholder-live-secret"));
        let without = |name: &str| {
            let v: Vec<_> = base.iter().filter(|(k, _)| *k != name).copied().collect();
            CoreConfig::from_env(&env(&v))
        };
        for name in ["FI_WEB_ORIGIN", "FI_EDGE_SECRET", "FI_PUBLIC_CORE_URL"] {
            assert!(without(name).is_err(), "{name} is required in prod");
        }
        let with = |name: &'static str, value: &str| {
            let mut v: Vec<_> = base.iter().filter(|(k, _)| *k != name).copied().collect();
            v.push((name, value));
            CoreConfig::from_env(&env(&v))
        };
        assert!(with("FI_TURNSTILE_MODE", "test").is_err());
        assert!(with("FI_TURNSTILE_SECRET", turnstile::TEST_SECRET_PASS).is_err());
        assert!(with("FI_WEB_ORIGIN", "http://app.example.test").is_err());
        assert!(with("FI_TURNSTILE_VERIFY_URL", "http://127.0.0.1:1/siteverify").is_err());
        assert!(with("FI_DISCORD_LINK_URL", "http://discord.example.test").is_err());
        assert!(with("FI_DISCORD_LINK_URL", "https://discord.example.test").is_ok());
        assert!(
            with("FI_MCP_ENABLED", "true").is_err(),
            "MCP needs its keys"
        );
    }
}
