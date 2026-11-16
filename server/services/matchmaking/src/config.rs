//! Settings of the matchmaking deployable, read once at startup. Tickets are verified with public
//! keys only (a configured `FI_TOKEN_SIGNING_KEYS` refuses the start); the one private key here
//! is the relay-grant ring, and its well-known local value is refused in prod.
//!
//! | Variable | Default | What |
//! | --- | --- | --- |
//! | `FI_TOKEN_VERIFY_KEYS` | required | `kid:base64(public key)` of the game access ticket |
//! | `FI_RELAY_GRANT_SIGNING_KEYS` | required | `kid:base64(32-byte seed)`, current first (relay-grant-signing-key) |
//! | `FI_GAMES` | `fh2` | games served |
//! | `FI_INSTANCE_ID` | hostname | `X-Fi-Instance`, the liveness rows' instance |
//! | `FI_GAME_LIVENESS_CACHE_MS` | 60000 | per-instance liveness cache, 0 to 60000 |
//! | `FI_EDGE_SECRET` | unset (no origin lock); required in prod | the origin lock (ADR 0903 §1) |
//! | `FI_MIN_HOOK` | unset (any hook) | the oldest hook served on the proxied game requests (GAME_PLATFORM.md §3a) |
//! | `FI_MM_SEAT_HOLD_SECS` | 600 | how long a reserved seat may be renewed (60 to 900; MATCHMAKING_API.md §3.2) |
//! | `FI_MM_OFFICIAL_WAIT_MS` | 10000 | how long a quick join or private create with no room waits for the Worlds agent to open a world before 503 `SERVICE.UNAVAILABLE` (0 to 15000; CR-S100, WORLDS_OFFICIAL.md §13) |
//! | `FI_MM_WORLD_IDLE_SECS` | 300 | a world with nobody but its host this long closes on its next heartbeat or `PUT` (`idle`; `0` off, else 60 to 86400; MATCHMAKING_API.md §3.3 item 4, OD-107) |
//! | `FI_RELAYS` | `[]` (off) | the relays this server trusts for the fallback (CR-S57, MATCHMAKING_API.md §5b): a JSON array of `{publicKey, endpoint: "host:binding/port", name}`, at most 32. List a relay only once it runs a CR-S57 image (an older relay refuses grants naming it) |
//! | `FI_RELAY_SEATS_PER_WORLD` | 4 | live relayed seats per Noob world (0 to 12; CR-S57 review M4; OD-160: Hardcore worlds uncapped) |
//! | `FI_RELAY_DIRECTORY_URL` | unset (no check) | the beholder's base URL (`https://host[:port]`; `http://` for loopback only): every instance reads its relay list every 30 s, and a Hardcore world opens direct when a fresh check did not list its relays (OD-160, MATCHMAKING_API.md §5b "Relay by mode"); unset, no relay counts as down |
//! | `FI_WORLDS` | `[]` (off) | the Worlds hosts this server trusts for official worlds (OD-188/189, MATCHMAKING_API.md §2.9): a JSON array of `{vmId, publicKey, endpoint: "ip:port", name, maxWorlds}`, at most 32 |
//! | `FI_EDGE_LOCATION_HEADERS` | `false` | `true` only when the edge's visitor location transform is on: the `CF-IP*` location headers are then trusted behind the origin lock (MATCHMAKING_API.md §3.5); otherwise every caller is unlocated |
//!
//! The pools a ticket may name are `fm5` when that game is served (CR-S97), `vip` and `matchmaking` ([`crate::POOLS`]: the gate's pool
//! names of `fh2`; per-game pool names are owner question PV1-Q3).

use fi_auth::keys::SigningKeys;
use fi_auth::relay_grant::PublicKeys;
use fi_runtime::config::ConfigError;
use fi_runtime::http::EdgeSecret;
use fi_surfaces::config::{Env, is_prod};
use std::sync::Arc;

/// At most this many Worlds hosts in `FI_WORLDS`.
pub const WORLDS_CONFIG_MAX: usize = 32;

/// `FI_MM_OFFICIAL_WAIT_MS`'s default (CR-S100): the agent sees a taken spare on its next announce
/// (3 s) and opens the next with one `PUT`, so 10 s covers about three announce periods, and fits
/// the hook's 20 s hold of the game's private `PUT` (WORLDS_OFFICIAL.md §12.2 item 2) and the
/// launcher's 30 s matchmaking timeout with room for the proof and Ready.
pub const DEFAULT_OFFICIAL_WAIT_MS: u64 = 10_000;

/// A Worlds host this server trusts (OD-188/189, MATCHMAKING_API.md §2.9): its agent signs every
/// call with `key`, and its official worlds announce `endpoint`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorldsHost {
    /// `[a-z0-9-]{1,32}`, unique.
    pub vm_id: String,
    /// The node key (Ed25519 public key), unique.
    pub key: [u8; 32],
    /// The engine's public IP literal (canonical text).
    pub ip: String,
    /// Its public UDP port.
    pub port: u16,
    /// The operator's name for it, 1-48 characters: the worlds' name and display host.
    pub name: String,
    /// Open official worlds at most (1-64).
    pub max_worlds: u32,
    /// The region it serves (OD-205, `[a-z0-9-]{1,32}`); absent: placement ignores regions for it.
    pub region: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorldsEntry {
    vm_id: String,
    public_key: String,
    endpoint: String,
    name: String,
    max_worlds: u32,
    #[serde(default)]
    region: Option<String>,
}

/// Parses `FI_WORLDS` (MATCHMAKING_API.md §2.9): `[{vmId, publicKey, endpoint, name, maxWorlds, region?}]`.
pub fn parse_worlds(text: &str) -> Result<Vec<WorldsHost>, ConfigError> {
    let bad = |reason| invalid("FI_WORLDS", reason);
    let entries: Vec<WorldsEntry> = serde_json::from_str(text)
        .map_err(|_| bad("a JSON array of {vmId, publicKey, endpoint, name, maxWorlds}"))?;
    if entries.len() > WORLDS_CONFIG_MAX {
        return Err(bad("at most 32 Worlds hosts"));
    }
    let mut out: Vec<WorldsHost> = Vec::with_capacity(entries.len());
    for e in entries {
        if !fi_protocol::matchmaking::is_vm_id(&e.vm_id) {
            return Err(bad("vmId: [a-z0-9-]{1,32}"));
        }
        let key: [u8; 32] = match fi_auth::b64::std_decode(&e.public_key) {
            Some(k) if e.public_key.len() == 44 => k
                .try_into()
                .map_err(|_| bad("publicKey: the standard base64 of 32 bytes"))?,
            _ => return Err(bad("publicKey: the standard base64 of 32 bytes")),
        };
        if ed25519_dalek::VerifyingKey::from_bytes(&key).is_err() {
            return Err(bad("publicKey: not an Ed25519 public key"));
        }
        if out.iter().any(|w| w.vm_id == e.vm_id) {
            return Err(bad("a vmId appears twice"));
        }
        if out.iter().any(|w| w.key == key) {
            return Err(bad("a publicKey appears twice"));
        }
        let Ok(addr) = e.endpoint.parse::<std::net::SocketAddr>() else {
            return Err(bad(
                "endpoint: an IP literal and a port (ip:port, [ipv6]:port)",
            ));
        };
        if addr.port() == 0 || addr.ip().is_unspecified() {
            return Err(bad(
                "endpoint: an IP literal and a port (ip:port, [ipv6]:port)",
            ));
        }
        let name = e.name.trim();
        if name.is_empty() || name.chars().count() > 48 || name.chars().any(char::is_control) {
            return Err(bad("name: 1 to 48 characters"));
        }
        if !(1..=64).contains(&e.max_worlds) {
            return Err(bad("maxWorlds: 1 to 64"));
        }
        if e.region
            .as_deref()
            .is_some_and(|r| !fi_protocol::matchmaking::is_vm_id(r))
        {
            return Err(bad("region: [a-z0-9-]{1,32}"));
        }
        out.push(WorldsHost {
            vm_id: e.vm_id,
            key,
            ip: addr.ip().to_string(),
            port: addr.port(),
            name: name.to_owned(),
            max_worlds: e.max_worlds,
            region: e.region,
        });
    }
    Ok(out)
}

/// The label whose SHA-256 is the local relay-grant seed of compose/game.yml (fi-game, ADR 0023).
pub const LOCAL_GRANT_LABEL: &str = "forza-infinity local dev relay-grant signing key 1";

/// `FI_RELAYS` by default: none, the fallback off (CR-S57 review M2: an older relay image refuses
/// any grant naming a relay, so an operator lists relays after deploying the new relay image).
pub const DEFAULT_RELAYS: &str = "[]";
/// At most this many relays in `FI_RELAYS`.
pub const RELAYS_CONFIG_MAX: usize = 32;

/// A relay this server trusts (CR-S57): what a grant names and a world shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relay {
    /// Public key (standard base64 of 32 bytes; the beholder identity).
    pub key: String,
    /// Host: an IPv4 literal or a DNS name.
    pub addr: String,
    /// Binding (STUN) UDP port.
    pub binding: u16,
    /// Rendezvous UDP port.
    pub port: u16,
    /// The operator's name for it, 1-48 characters.
    pub name: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RelayEntry {
    public_key: String,
    endpoint: String,
    name: String,
}

fn host_ok(host: &str) -> bool {
    (1..=253).contains(&host.len())
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
}

/// Parses `FI_RELAYS` (CR-S57): `[{publicKey, endpoint: "host:binding/port", name}]`.
pub fn parse_relays(text: &str) -> Result<Vec<Relay>, ConfigError> {
    let bad = |reason| invalid("FI_RELAYS", reason);
    let entries: Vec<RelayEntry> = serde_json::from_str(text)
        .map_err(|_| bad("a JSON array of {publicKey, endpoint, name}"))?;
    if entries.len() > RELAYS_CONFIG_MAX {
        return Err(bad("at most 32 relays"));
    }
    let mut out: Vec<Relay> = Vec::with_capacity(entries.len());
    for e in entries {
        let key_ok = fi_auth::b64::std_decode(&e.public_key).is_some_and(|k| k.len() == 32)
            && e.public_key.len() == 44;
        if !key_ok {
            return Err(bad("publicKey: the standard base64 of 32 bytes"));
        }
        if out.iter().any(|r| r.key == e.public_key) {
            return Err(bad("a publicKey appears twice"));
        }
        let parsed = e.endpoint.rsplit_once(':').and_then(|(host, ports)| {
            let (binding, port) = ports.split_once('/')?;
            let binding: u16 = binding.parse().ok().filter(|p| *p != 0)?;
            let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
            host_ok(host).then(|| (host.to_owned(), binding, port))
        });
        let Some((addr, binding, port)) = parsed else {
            return Err(bad("endpoint: host:bindingPort/rendezvousPort"));
        };
        let name = e.name.trim();
        if name.is_empty() || name.chars().count() > 48 || name.chars().any(char::is_control) {
            return Err(bad("name: 1 to 48 characters"));
        }
        out.push(Relay {
            key: e.public_key,
            addr,
            binding,
            port,
            name: name.to_owned(),
        });
    }
    Ok(out)
}

/// The deployment's settings.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Validated global launcher compatibility floor.
    pub min_launcher: String,
    /// Required content policy; explicit local synthetic-test bypass only.
    pub content_required: bool,
    /// Ticket public keys.
    pub verify_keys: PublicKeys,
    /// Relay-grant signing ring.
    pub grant_keys: SigningKeys,
    /// Games served.
    pub games: Vec<String>,
    /// This instance.
    pub instance_id: String,
    /// Liveness cache TTL (ms).
    pub liveness_ms: u32,
    /// The pools a ticket may name (the gate's pool names; [`crate::POOLS`] by default).
    pub pools: Vec<String>,
    /// The origin lock.
    pub edge: Option<Arc<EdgeSecret>>,
    /// `FI_MIN_HOOK`: the oldest hook the proxied game requests accept (`None`: any).
    pub min_hook: Option<String>,
    /// `FI_MM_SEAT_HOLD_SECS`: how long a reserved seat may be renewed.
    pub seat_hold_secs: i64,
    /// `FI_MM_WORLD_IDLE_SECS`: how long a world may have nobody but its host (`None`: off).
    pub world_idle_secs: Option<i64>,
    /// `FI_MM_OFFICIAL_WAIT_MS`: the bounded wait for an official world (CR-S100).
    pub official_wait_ms: u64,
    /// `FI_EDGE_LOCATION_HEADERS`: trust the edge's visitor location headers (default false).
    pub location_headers: bool,
    /// `FI_RELAYS`: the relays this server trusts, in preference order (CR-S57).
    pub relays: Vec<Relay>,
    /// `FI_RELAY_SEATS_PER_WORLD`: live relayed seats per world.
    pub relay_seats: u32,
    /// `FI_RELAY_DIRECTORY_URL`: the beholder's base URL, without a trailing slash (OD-160).
    pub relay_directory: Option<String>,
    /// `FI_WORLDS`: the Worlds hosts this server trusts (OD-188/189).
    pub worlds: Vec<WorldsHost>,
}

/// `FI_RELAY_DIRECTORY_URL` (OD-160): `https://host[:port]` (`http://` only for a loopback host:
/// an on-path attacker could otherwise mark relays down), no path, query or user info; a trailing
/// slash is dropped.
pub fn parse_relay_directory(text: &str) -> Result<String, ConfigError> {
    let bad = |reason| invalid("FI_RELAY_DIRECTORY_URL", reason);
    let base = text.trim().trim_end_matches('/');
    let rest = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .ok_or_else(|| bad("http:// or https:// and a host"))?;
    let host = match rest.find(']') {
        Some(end) if rest.starts_with('[') => &rest[..=end],
        _ => rest.split(':').next().unwrap_or(rest),
    };
    if base.starts_with("http://") && !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(bad("https://, or http:// for a loopback host only"));
    }
    if rest.is_empty()
        || rest
            .chars()
            .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')))
    {
        return Err(bad(
            "a host and an optional port, no path, query or user info",
        ));
    }
    Ok(base.to_owned())
}

fn invalid(name: &'static str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.to_owned(),
    }
}

impl Settings {
    /// Reads and checks the settings.
    pub fn from_env(env: &dyn Env) -> Result<Self, ConfigError> {
        if env.get("FI_TOKEN_SIGNING_KEYS").is_some() {
            return Err(invalid(
                "FI_TOKEN_SIGNING_KEYS",
                "matchmaking verifies tickets and never mints them: configure FI_TOKEN_VERIFY_KEYS only",
            ));
        }
        let verify_keys = PublicKeys::parse(
            "FI_TOKEN_VERIFY_KEYS",
            &env.get("FI_TOKEN_VERIFY_KEYS")
                .ok_or(ConfigError::Missing("FI_TOKEN_VERIFY_KEYS"))?,
        )
        .map_err(|e| invalid("FI_TOKEN_VERIFY_KEYS", e.reason))?;
        let grant_keys = SigningKeys::parse(
            "FI_RELAY_GRANT_SIGNING_KEYS",
            &env.get("FI_RELAY_GRANT_SIGNING_KEYS")
                .ok_or(ConfigError::Missing("FI_RELAY_GRANT_SIGNING_KEYS"))?,
        )
        .map_err(|e| invalid("FI_RELAY_GRANT_SIGNING_KEYS", e.reason))?;
        let local = fi_auth::sha256(LOCAL_GRANT_LABEL.as_bytes());
        let has_local = grant_keys.public_keys().iter().any(|(_, pk)| {
            SigningKeys::from_seeds("X", vec![("l".into(), local)])
                .map(|r| r.public_keys()[0].1 == *pk)
                .unwrap_or(false)
        });
        if is_prod(env) && (has_local || grant_keys.has_local_dev_key()) {
            return Err(invalid(
                "FI_RELAY_GRANT_SIGNING_KEYS",
                "a well-known local development key is refused in prod",
            ));
        }
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
        // A ticket may also name the own pool of a served game that has one (CR-S97: FM5).
        let game_pools: Vec<String> = games
            .iter()
            .filter(|g| fi_domain::GameRules::of(g).pools == fi_domain::game::Pools::Own)
            .cloned()
            .collect();
        let liveness_ms: u32 = match env.get("FI_GAME_LIVENESS_CACHE_MS") {
            None => fi_surfaces::game::cache::MAX_TTL_MS,
            Some(v) => v
                .trim()
                .parse()
                .ok()
                .filter(|ms| *ms <= fi_surfaces::game::cache::MAX_TTL_MS)
                .ok_or_else(|| invalid("FI_GAME_LIVENESS_CACHE_MS", "0 to 60000"))?,
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
        let seat_hold_secs: i64 = match env.get("FI_MM_SEAT_HOLD_SECS") {
            None => fi_db::matchmaking::DEFAULT_SEAT_HOLD_SECS,
            Some(v) => v
                .trim()
                .parse()
                .ok()
                .filter(|s| (60..=900).contains(s))
                .ok_or_else(|| invalid("FI_MM_SEAT_HOLD_SECS", "60 to 900"))?,
        };
        let world_idle_secs: Option<i64> = match env.get("FI_MM_WORLD_IDLE_SECS") {
            None => Some(fi_db::matchmaking::DEFAULT_WORLD_IDLE_SECS),
            Some(v) => match v.trim().parse::<i64>().ok() {
                Some(0) => None,
                Some(s) if (60..=86_400).contains(&s) => Some(s),
                _ => return Err(invalid("FI_MM_WORLD_IDLE_SECS", "0 (off) or 60 to 86400")),
            },
        };
        let official_wait_ms: u64 = match env.get("FI_MM_OFFICIAL_WAIT_MS") {
            None => DEFAULT_OFFICIAL_WAIT_MS,
            Some(v) => v
                .trim()
                .parse()
                .ok()
                .filter(|ms| *ms <= 15_000)
                .ok_or_else(|| invalid("FI_MM_OFFICIAL_WAIT_MS", "0 to 15000"))?,
        };
        let location_headers = match env
            .get("FI_EDGE_LOCATION_HEADERS")
            .as_deref()
            .map(str::trim)
        {
            None | Some("") | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(invalid("FI_EDGE_LOCATION_HEADERS", "true or false")),
        };
        let relays = parse_relays(
            env.get("FI_RELAYS")
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(DEFAULT_RELAYS),
        )?;
        let relay_seats: u32 = match env.get("FI_RELAY_SEATS_PER_WORLD") {
            None => fi_db::matchmaking::DEFAULT_RELAY_SEATS_PER_WORLD,
            Some(v) => v
                .trim()
                .parse()
                .ok()
                .filter(|n| *n <= 12)
                .ok_or_else(|| invalid("FI_RELAY_SEATS_PER_WORLD", "0 to 12"))?,
        };
        let relay_directory = env
            .get("FI_RELAY_DIRECTORY_URL")
            .filter(|v| !v.trim().is_empty())
            .map(|v| parse_relay_directory(&v))
            .transpose()?;
        let worlds = parse_worlds(
            env.get("FI_WORLDS")
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or("[]"),
        )?;
        Ok(Self {
            relays,
            worlds,
            relay_seats,
            relay_directory,
            min_launcher: fi_surfaces::config::launcher_versions_from_env(env)?.0,
            content_required: fi_surfaces::config::content_required_from_env(env)?,
            seat_hold_secs,
            world_idle_secs,
            official_wait_ms,
            location_headers,
            verify_keys,
            grant_keys,
            games,
            instance_id,
            liveness_ms,
            pools: crate::POOLS
                .iter()
                .map(|p| (*p).to_owned())
                .chain(game_pools)
                .collect(),
            edge: fi_surfaces::config::edge_secret(env)?,
            min_hook: fi_surfaces::game::proxy::min_hook_from_env(env)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One synthetic relay: a documentation address and a throwaway key.
    fn test_relays() -> String {
        r#"[{"publicKey":"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=","endpoint":"203.0.113.10:3478/5101","name":"Test Relay"}]"#.to_owned()
    }

    /// compose/game.yml's local values: the token ring's public half and the relay seed.
    pub const LOCAL_VERIFY_KEYS: &str = "at-local-1:NRMFDrDXMyz5ESomMUulIiSVrVUYWQ+IPBFTk4zYw30=";

    fn local_grant_ring() -> String {
        format!(
            "rg-local-1:{}",
            fi_auth::b64::std_encode(&fi_auth::sha256(LOCAL_GRANT_LABEL.as_bytes()))
        )
    }

    fn env(extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        let mut v: Vec<(&'static str, String)> = vec![
            ("FI_TOKEN_VERIFY_KEYS", LOCAL_VERIFY_KEYS.into()),
            ("FI_RELAY_GRANT_SIGNING_KEYS", local_grant_ring()),
        ];
        for (k, val) in extra {
            v.retain(|(n, _)| n != k);
            v.push((k, val.clone()));
        }
        v
    }

    #[test]
    fn the_official_wait_is_bounded() {
        let s = Settings::from_env(&env(&[])).unwrap();
        assert_eq!(s.official_wait_ms, DEFAULT_OFFICIAL_WAIT_MS);
        for (v, ms) in [
            ("0", Some(0)),
            ("15000", Some(15_000)),
            ("15001", None),
            ("x", None),
        ] {
            let got = Settings::from_env(&env(&[("FI_MM_OFFICIAL_WAIT_MS", v.into())]))
                .ok()
                .map(|s| s.official_wait_ms);
            assert_eq!(got, ms, "{v}");
        }
    }

    #[test]
    fn verify_only_and_the_local_relay_key_stays_local() {
        let s = Settings::from_env(&env(&[])).unwrap();
        assert_eq!(s.games, vec!["fh2".to_owned()]);
        assert_eq!(s.min_hook, None);
        assert_eq!(s.seat_hold_secs, 600);
        assert_eq!(
            Settings::from_env(&env(&[("FI_MM_SEAT_HOLD_SECS", "120".into())]))
                .unwrap()
                .seat_hold_secs,
            120
        );
        assert!(Settings::from_env(&env(&[("FI_MM_SEAT_HOLD_SECS", "30".into())])).is_err());
        assert_eq!(
            s.world_idle_secs,
            Some(300),
            "five minutes by default (OD-107)"
        );
        let idle = |v: &str| {
            Settings::from_env(&env(&[("FI_MM_WORLD_IDLE_SECS", v.into())]))
                .map(|s| s.world_idle_secs)
        };
        assert_eq!(idle("0").unwrap(), None, "0 turns the rule off");
        assert_eq!(idle(" 600 ").unwrap(), Some(600));
        assert_eq!(idle("60").unwrap(), Some(60));
        assert_eq!(idle("86400").unwrap(), Some(86_400));
        assert_eq!(idle("").unwrap(), Some(300), "empty is unset");
        for bad in ["59", "86401", "-1", "5m"] {
            assert!(idle(bad).is_err(), "{bad}");
        }
        assert!(!s.location_headers, "untrusted unless the edge says so");
        assert!(
            Settings::from_env(&env(&[("FI_EDGE_LOCATION_HEADERS", "true".into())]))
                .unwrap()
                .location_headers
        );
        assert!(Settings::from_env(&env(&[("FI_EDGE_LOCATION_HEADERS", "yes".into())])).is_err());
        assert!(Settings::from_env(&env(&[("FI_MIN_HOOK", "x".into())])).is_err());
        assert!(Settings::from_env(&env(&[("FI_TOKEN_SIGNING_KEYS", "at-1:x".into())])).is_err());
        let prod = Settings::from_env(&env(&[("APP_ENV", "prod".into())])).unwrap_err();
        assert!(prod.to_string().contains("local development key"), "{prod}");
        let other = format!("rg-2026-10:{}", fi_auth::b64::std_encode(&[9u8; 32]));
        assert!(
            Settings::from_env(&env(&[
                ("APP_ENV", "prod".into()),
                ("FI_RELAY_GRANT_SIGNING_KEYS", other.clone()),
                ("FI_EDGE_SECRET", "e".repeat(32)),
            ]))
            .is_ok()
        );
        // Prod refuses to start without the origin lock.
        match Settings::from_env(&env(&[
            ("APP_ENV", "prod".into()),
            ("FI_RELAY_GRANT_SIGNING_KEYS", other),
        ])) {
            Err(ConfigError::Missing(n)) => assert_eq!(n, "FI_EDGE_SECRET"),
            e => panic!("{e:?}"),
        }
        // OD-188/189: no Worlds host by default; FI_WORLDS is validated at start.
        let pk = "A6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg=";
        let worlds =
            |v: &str| Settings::from_env(&env(&[("FI_WORLDS", v.into())])).map(|s| s.worlds);
        assert_eq!(worlds("").unwrap(), vec![]);
        assert_eq!(Settings::from_env(&env(&[])).unwrap().worlds, vec![]);
        let one = worlds(&format!(
            r#"[{{"vmId":"local-mac","publicKey":"{pk}","endpoint":"192.168.1.53:5200","name":"Local","maxWorlds":4}}]"#
        ))
        .unwrap();
        assert_eq!(
            (
                one[0].vm_id.as_str(),
                one[0].ip.as_str(),
                one[0].port,
                one[0].max_worlds
            ),
            ("local-mac", "192.168.1.53", 5200, 4)
        );
        assert_eq!(
            worlds(&format!(
                r#"[{{"vmId":"v6","publicKey":"{pk}","endpoint":"[2001:db8::1]:5200","name":"V6","maxWorlds":1}}]"#
            ))
            .unwrap()[0]
                .ip,
            "2001:db8::1"
        );
        let other = fi_auth::b64::std_encode(
            ed25519_dalek::SigningKey::from_bytes(&[5u8; 32])
                .verifying_key()
                .as_bytes(),
        );
        let entry = |vm: &str, key: &str, endpoint: &str, name: &str, max: i64| {
            format!(
                r#"{{"vmId":"{vm}","publicKey":"{key}","endpoint":"{endpoint}","name":"{name}","maxWorlds":{max}}}"#
            )
        };
        for bad in [
            "{}".to_owned(),
            "[1]".to_owned(),
            format!("[{}]", entry("Local", pk, "192.168.1.53:5200", "n", 1)),
            format!("[{}]", entry("", pk, "192.168.1.53:5200", "n", 1)),
            format!("[{}]", entry("a", "c2hvcnQ=", "192.168.1.53:5200", "n", 1)),
            format!("[{}]", entry("a", pk, "worlds.example.com:5200", "n", 1)),
            format!("[{}]", entry("a", pk, "192.168.1.53", "n", 1)),
            format!("[{}]", entry("a", pk, "192.168.1.53:0", "n", 1)),
            format!("[{}]", entry("a", pk, "0.0.0.0:5200", "n", 1)),
            format!("[{}]", entry("a", pk, "192.168.1.53:5200", "", 1)),
            format!("[{}]", entry("a", pk, "192.168.1.53:5200", "n", 0)),
            format!("[{}]", entry("a", pk, "192.168.1.53:5200", "n", 65)),
            format!(
                "[{},{}]",
                entry("a", pk, "192.168.1.53:5200", "n", 1),
                entry("a", &other, "192.168.1.53:5201", "n", 1)
            ),
            format!(
                "[{},{}]",
                entry("a", pk, "192.168.1.53:5200", "n", 1),
                entry("b", pk, "192.168.1.53:5201", "n", 1)
            ),
            format!(
                "[{}]",
                (0..33)
                    .map(|i| entry(&format!("v{i}"), pk, "192.168.1.53:5200", "n", 1))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            r#"[{"vmId":"a","publicKey":"x","endpoint":"1.2.3.4:5","name":"n","maxWorlds":1,"extra":1}]"#.to_owned(),
        ] {
            match worlds(&bad) {
                Err(e) => assert!(e.to_string().contains("FI_WORLDS"), "{bad}: {e}"),
                Ok(w) => panic!("{bad}: {w:?}"),
            }
        }
        // CR-S57 review M2: no relay by default; an operator lists relays in FI_RELAYS.
        assert!(s.relays.is_empty());
        assert_eq!(s.relay_seats, 4);
        assert!(Settings::from_env(&env(&[("FI_RELAY_SEATS_PER_WORLD", "13".into())])).is_err());
        assert_eq!(
            parse_relays(&test_relays()).unwrap(),
            vec![Relay {
                key: "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=".into(),
                addr: "203.0.113.10".into(),
                binding: 3478,
                port: 5101,
                name: "Test Relay".into(),
            }]
        );
        let relays =
            |v: &str| Settings::from_env(&env(&[("FI_RELAYS", v.into())])).map(|s| s.relays);
        assert!(relays("[]").unwrap().is_empty());
        let two = format!(
            r#"[{{"publicKey":"{}","endpoint":"relay.example.org:3478/5101","name":"Community"}},{}]"#,
            fi_auth::b64::std_encode(&[7u8; 32]),
            &test_relays()[1..test_relays().len() - 1]
        );
        let parsed = relays(&two).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].addr, "relay.example.org");
        for bad in [
            "x",
            r#"[{"publicKey":"AA==","endpoint":"1.2.3.4:3478/5101","name":"n"}]"#,
            r#"[{"publicKey":"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=","endpoint":"1.2.3.4:3478","name":"n"}]"#,
            r#"[{"publicKey":"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=","endpoint":"1.2.3.4:0/5101","name":"n"}]"#,
            r#"[{"publicKey":"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=","endpoint":"a b:1/2","name":"n"}]"#,
            r#"[{"publicKey":"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=","endpoint":"1.2.3.4:1/2","name":""}]"#,
            r#"[{"publicKey":"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=","endpoint":"1.2.3.4:1/2","name":"n","x":1}]"#,
        ] {
            assert!(relays(bad).is_err(), "{bad}");
        }
        let dup = format!("[{0},{0}]", &test_relays()[1..test_relays().len() - 1]);
        assert!(relays(&dup).is_err(), "a key twice");
    }

    /// OD-160: the beholder's base URL: https, or http to a loopback host only; no path.
    #[test]
    fn the_relay_directory_is_https_or_loopback() {
        for (text, want) in [
            (
                "https://beholder.example.org",
                "https://beholder.example.org",
            ),
            (
                "https://beholder.example.org:8443/",
                "https://beholder.example.org:8443",
            ),
            ("http://127.0.0.1:4100", "http://127.0.0.1:4100"),
            ("http://localhost", "http://localhost"),
            ("http://[::1]:4100", "http://[::1]:4100"),
        ] {
            assert_eq!(parse_relay_directory(text).unwrap(), want, "{text}");
        }
        for bad in [
            "http://beholder.example.org",
            "http://192.168.1.2:4100",
            "ftp://beholder.example.org",
            "https://",
            "https://beholder.example.org/beholder/v1",
            "https://user@beholder.example.org",
            "https://beholder.example.org?x=1",
            "beholder.example.org",
        ] {
            assert!(parse_relay_directory(bad).is_err(), "{bad}");
        }
    }
}
