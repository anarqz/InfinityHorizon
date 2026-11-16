//! The server's announces to the beholder (docs/contracts/ANNOUNCER_API.md §2, CR-S54): optional,
//! on only when `FI_BEHOLDER_URL` is set. One announcer per process that sets it, so only the
//! instance behind the public URL sets it (a Cloud Run split sets it on one service, `fi-infinity`).
//!
//! | Variable | Default | What |
//! | --- | --- | --- |
//! | `FI_BEHOLDER_URL` | unset (off) | the beholder's base URL, `https://beholder.<domain>` |
//! | `FI_ANNOUNCE_NAME` | the endpoint's host | the listing's name, 1-48 characters |
//! | `FI_ANNOUNCE_ENDPOINT` | `FI_PUBLIC_CORE_URL` (the compose fills it from `FI_PUBLIC_URL`) | `http(s)://host[:port]` |
//! | `FI_ANNOUNCE_REGION` | empty | 0-16 of `[A-Za-z0-9-]` |
//! | `FI_ANNOUNCE_CAPACITY` | the game's online capacity, else the sum of `FI_POOLS_JSON`'s capacities | 1-1000 |
//! | `FI_ANNOUNCE_SIGNING_KEY` | `<FI_KEYS_DIR>/FI_ANNOUNCE_SIGNING_KEY` (`keys init`) | the node key (secret) |
//!
//! The version is the binary's (`CARGO_PKG_VERSION`, plus the Cloud Run revision when there is one).
//! Nothing here logs the URL, the endpoint or the key.

use fi_announce::{Announcer, Kind, Node, NodeKey};
use fi_db::PgPool;
use fi_runtime::config::ConfigError;
use fi_surfaces::config::Env;

/// The announcer's settings, when `FI_BEHOLDER_URL` is set.
pub struct Settings {
    announcer: Announcer,
    node: Node,
    /// `FI_ANNOUNCE_CAPACITY`, clamped.
    fixed_capacity: Option<u32>,
    /// The sum of `FI_POOLS_JSON`'s capacities for the game, clamped (unlimited: 1000).
    pools_capacity: u32,
    game: String,
}

fn invalid(name: &'static str, reason: &str) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

/// The settings from `env`: `None` when `FI_BEHOLDER_URL` is unset (no announcer, nothing read).
pub fn settings(env: &dyn Env) -> Result<Option<Settings>, ConfigError> {
    let Some(url) = env.get("FI_BEHOLDER_URL") else {
        return Ok(None);
    };
    let key = NodeKey::parse(
        &env.get("FI_ANNOUNCE_SIGNING_KEY")
            .ok_or(ConfigError::Missing("FI_ANNOUNCE_SIGNING_KEY"))?,
    )
    .map_err(|_| invalid("FI_ANNOUNCE_SIGNING_KEY", "not base64 of 32 bytes"))?;
    let announcer =
        Announcer::new(&url, key).map_err(|_| invalid("FI_BEHOLDER_URL", "an http(s) URL"))?;
    let endpoint = env
        .get("FI_ANNOUNCE_ENDPOINT")
        .or_else(|| env.get("FI_PUBLIC_CORE_URL"))
        .ok_or(ConfigError::Missing("FI_ANNOUNCE_ENDPOINT"))?
        .trim()
        .trim_end_matches('/')
        .to_owned();
    let host = fi_announce::endpoint_host(Kind::Server, &endpoint)
        .ok_or_else(|| invalid("FI_ANNOUNCE_ENDPOINT", "http(s)://host[:port], no path"))?;
    let name = env
        .get("FI_ANNOUNCE_NAME")
        .map(|n| n.trim().to_owned())
        .unwrap_or_else(|| host.chars().take(48).collect());
    let fixed_capacity = env
        .get("FI_ANNOUNCE_CAPACITY")
        .map(|c| {
            c.trim()
                .parse::<u64>()
                .map(|c| fi_announce::clamp_capacity(Some(c)))
                .map_err(|_| invalid("FI_ANNOUNCE_CAPACITY", "a number"))
        })
        .transpose()?;
    let game = env
        .get("FI_GAMES")
        .and_then(|g| {
            g.split(',')
                .map(str::trim)
                .find(|g| !g.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "fh2".to_owned());
    let pools_capacity = pools_capacity(env.get("FI_POOLS_JSON").as_deref(), &game);
    let node = Node {
        kind: Kind::Server,
        endpoint,
        name,
        region: env
            .get("FI_ANNOUNCE_REGION")
            .map(|r| r.trim().to_owned())
            .unwrap_or_default(),
        version: fi_announce::version_string(env!("CARGO_PKG_VERSION"), ""),
        capacity: fixed_capacity.unwrap_or(pools_capacity),
        rendezvous_port: None,
    };
    node.validate().map_err(|field| match field {
        "name" => invalid("FI_ANNOUNCE_NAME", "1-48 characters, no control character"),
        "region" => invalid("FI_ANNOUNCE_REGION", "0-16 of A-Z, a-z, 0-9 and -"),
        "endpoint" => invalid("FI_ANNOUNCE_ENDPOINT", "http(s)://host[:port], no path"),
        _ => invalid("FI_ANNOUNCE_CAPACITY", "1-1000"),
    })?;
    Ok(Some(Settings {
        announcer,
        node,
        fixed_capacity,
        pools_capacity,
        game,
    }))
}

/// The sum of the capacities of `game`'s pools in `FI_POOLS_JSON`, clamped to 1-1000 (an unlimited
/// pool, no pool or unreadable JSON: 1000).
fn pools_capacity(json: Option<&str>, game: &str) -> u32 {
    let Some(pools) = json.and_then(|j| fi_db::admission::parse_pools_json(j).ok()) else {
        return fi_announce::clamp_capacity(None);
    };
    let mut sum = 0u64;
    let mut any = false;
    for p in pools.iter().filter(|p| p.game == game) {
        match p.capacity {
            Some(c) => sum += u64::from(c),
            None => return fi_announce::clamp_capacity(None),
        }
        any = true;
    }
    fi_announce::clamp_capacity(any.then_some(sum))
}

impl Settings {
    /// The listing's name (startup log: a name is public, not an endpoint).
    pub fn name(&self) -> &str {
        &self.node.name
    }

    /// Announces until `stop`: the capacity is `FI_ANNOUNCE_CAPACITY`, else the game's online
    /// capacity (bounded by the global one, OD-228) read before each post (an operator's `online-capacity --set` shows within 30 s),
    /// else the pools' sum.
    pub async fn run(
        mut self,
        pool: PgPool,
        revision: String,
        stop: impl std::future::Future<Output = ()>,
    ) {
        self.node.version = fi_announce::version_string(env!("CARGO_PKG_VERSION"), &revision);
        let (fixed, pools, game) = (self.fixed_capacity, self.pools_capacity, self.game);
        let capacity = move || {
            let pool = pool.clone();
            let game = game.clone();
            async move {
                if let Some(c) = fixed {
                    return c;
                }
                online_capacity(&pool, &game).await.unwrap_or(pools)
            }
        };
        self.announcer.run(self.node, capacity, stop).await;
    }
}

/// The game's online capacity, bounded by the global one across every game (OD-228), when either
/// is set (`None`: both unlimited, unknown game or unreadable).
async fn online_capacity(pool: &PgPool, game: &str) -> Option<u32> {
    let mut conn = pool.acquire().await.ok()?;
    let own = match fi_db::gate::online_capacity_in(&mut conn, game).await {
        Ok(Some(c)) => c,
        _ => return None,
    };
    let global = fi_db::gate::global_online_capacity_in(&mut conn)
        .await
        .ok()
        .flatten();
    let c = match (own, global) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b)?,
    };
    Some(fi_announce::clamp_capacity(Some(u64::from(c))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=";
    const POOLS: &str = r#"[{"name":"vip","tier":"vip","capacity":8,"queueLimit":100,"instanceCapacity":4,"gameEndpoint":"http://127.0.0.1:8080"},{"name":"matchmaking","tier":"free","capacity":16,"queueLimit":100,"instanceCapacity":4,"gameEndpoint":"http://127.0.0.1:8080"}]"#;

    fn env(pairs: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn off_without_a_beholder_url() {
        // Nothing else is read: a missing key or endpoint does not matter.
        assert!(settings(&env(&[])).unwrap().is_none());
        assert!(
            settings(&env(&[("FI_ANNOUNCE_NAME", "x"), ("FI_POOLS_JSON", POOLS)]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_live_server_announces_with_the_owner_name() {
        let s = settings(&env(&[
            ("FI_BEHOLDER_URL", "https://beholder.example.test"),
            ("FI_ANNOUNCE_SIGNING_KEY", KEY),
            ("FI_PUBLIC_CORE_URL", "https://play.example.test"),
            ("FI_ANNOUNCE_NAME", "Example Beta (play.example.test)"),
            ("FI_POOLS_JSON", POOLS),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(s.node.endpoint, "https://play.example.test");
        assert_eq!(s.name(), "Example Beta (play.example.test)");
        assert_eq!(
            (s.node.capacity, s.pools_capacity, s.fixed_capacity),
            (24, 24, None)
        );
        assert_eq!(s.game, "fh2");
        assert_eq!(s.node.region, "");
    }

    #[test]
    fn defaults_and_refusals() {
        let base = [
            ("FI_BEHOLDER_URL", "https://beholder.example.com"),
            ("FI_ANNOUNCE_SIGNING_KEY", KEY),
        ];
        let with = |extra: &[(&'static str, &str)]| {
            let mut e = env(&base);
            e.extend(env(extra));
            settings(&e)
        };
        // The endpoint's host names it; no pools: 1000; an explicit endpoint and capacity win.
        let s = with(&[("FI_PUBLIC_CORE_URL", "http://192.168.1.40:8080/")])
            .unwrap()
            .unwrap();
        assert_eq!((s.name(), s.node.capacity), ("192.168.1.40", 1000));
        assert_eq!(s.node.endpoint, "http://192.168.1.40:8080");
        let s = with(&[
            ("FI_PUBLIC_CORE_URL", "http://127.0.0.1:8080"),
            ("FI_ANNOUNCE_ENDPOINT", "https://play.example.com"),
            ("FI_ANNOUNCE_CAPACITY", "5000"),
            ("FI_ANNOUNCE_REGION", "test-east"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(s.node.endpoint, "https://play.example.com");
        assert_eq!(s.fixed_capacity, Some(1000));
        let refused = |extra: &[(&'static str, &str)]| match with(extra) {
            Err(ConfigError::Invalid { name, .. }) => name,
            Err(ConfigError::Missing(name)) => name,
            Ok(_) => "accepted",
        };
        assert_eq!(refused(&[]), "FI_ANNOUNCE_ENDPOINT");
        let ok = ("FI_ANNOUNCE_ENDPOINT", "https://play.example.com");
        assert_eq!(
            refused(&[("FI_ANNOUNCE_ENDPOINT", "https://play.example.com/api")]),
            "FI_ANNOUNCE_ENDPOINT"
        );
        assert_eq!(
            refused(&[ok, ("FI_ANNOUNCE_NAME", &"x".repeat(49))]),
            "FI_ANNOUNCE_NAME"
        );
        assert_eq!(
            refused(&[ok, ("FI_ANNOUNCE_REGION", "sa east")]),
            "FI_ANNOUNCE_REGION"
        );
        assert_eq!(
            refused(&[ok, ("FI_ANNOUNCE_CAPACITY", "many")]),
            "FI_ANNOUNCE_CAPACITY"
        );
        let mut no_key = env(&[("FI_BEHOLDER_URL", "https://beholder.example.com"), ok]);
        assert!(matches!(
            settings(&no_key),
            Err(ConfigError::Missing("FI_ANNOUNCE_SIGNING_KEY"))
        ));
        no_key.push(("FI_ANNOUNCE_SIGNING_KEY", "short".into()));
        assert!(settings(&no_key).is_err());
        let bad_url = env(&[
            ("FI_BEHOLDER_URL", "beholder.example.com"),
            ("FI_ANNOUNCE_SIGNING_KEY", KEY),
            ok,
        ]);
        assert!(matches!(
            settings(&bad_url),
            Err(ConfigError::Invalid {
                name: "FI_BEHOLDER_URL",
                ..
            })
        ));
    }

    #[test]
    fn pools_capacity_sums_the_game_and_treats_unlimited_as_the_maximum() {
        assert_eq!(pools_capacity(Some(POOLS), "fh2"), 24);
        assert_eq!(pools_capacity(Some(POOLS), "other"), 1000);
        assert_eq!(pools_capacity(None, "fh2"), 1000);
        assert_eq!(pools_capacity(Some("not json"), "fh2"), 1000);
        let unlimited = POOLS.replace(r#""capacity":8,"#, "");
        assert_eq!(pools_capacity(Some(&unlimited), "fh2"), 1000);
    }
}
