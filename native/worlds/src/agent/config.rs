// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The world agent's settings (DEDICATED_SERVER.md §16): each option has an `FI_WORLDS_*`
//! environment twin; precedence CLI, then environment, then the default. A bad value refuses the
//! start with one `agent.config.invalid {field}` line naming the environment variable, never its
//! value. The node key file is loaded (or created once, 0600) by [`load_or_create_key`].

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use ed25519_dalek::SigningKey;

use crate::core::keys::{decode_base64, encode_base64};

/// The settings' names in `agent.config.invalid {field}`.
pub type Invalid = &'static str;

/// The activities an official world may run (MATCHMAKING_API §2.9 `OfficialWorldSettings`;
/// WORLDS_OFFICIAL.md §11.1): `freeroam` and `roadtrip` mixed (OD-210, CR-S101), `carmeet` for
/// both modes.
pub const ACTIVITIES: [&str; 3] = ["freeroam", "carmeet", "roadtrip"];
/// The car meet lobby, which admits both modes by its activity (OD-180, WORLDS_OFFICIAL.md
/// §11.1): its spare keeps the agent's first mode as `mode`, as before OD-210.
pub const MIXED: &str = "carmeet";
/// The default `--max-worlds` (OD-218, W13; WORLDS_OFFICIAL.md §13): the safety ceiling of a
/// 1 vCPU / 949 MB host (sao-1), above what its 100 FH2 players fill: at least 9 full worlds,
/// one per activity partly filled, and the warm spares (three clean, three pool), with room
/// for private lobbies. `--max-seats` 20 × 12 = 240 (20 × 16 = 320 when `race` is placed) and
/// `--memory-limit-mb` 20 × 28 + 20 = 580 (§8).
pub const DEFAULT_MAX_WORLDS: u64 = 20;
/// The games with official pool worlds (WORLDS_OFFICIAL.md §14, CR-S103): FH2's flagged
/// sessions play in pool worlds; FM5 has none (a flagged FM5 seat is ended instead).
pub const POOL_GAMES: [&str; 1] = [GAME];

/// A placement group the agent keeps one warm spare of (W13, W17): its mode and activity, and
/// whether its worlds are pool worlds (`OfficialWorldSettings.pool`, WORLDS_OFFICIAL.md §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Spare {
    pub mode: &'static str,
    pub activity: &'static str,
    pub pool: bool,
}

/// Every group placed: the clean ones ([`spare_groups`]), then, with `pool`, the same groups of
/// the [`POOL_GAMES`] as pool worlds (clean spares come first under the cap).
pub fn spares(modes: &[&'static str], activities: &[&'static str], pool: bool) -> Vec<Spare> {
    let groups = spare_groups(modes, activities);
    let clean = groups.iter().map(|&(mode, activity)| Spare {
        mode,
        activity,
        pool: false,
    });
    let pools = groups
        .iter()
        .filter(|(_, a)| pool && POOL_GAMES.contains(&game_of(a)))
        .map(|&(mode, activity)| Spare {
            mode,
            activity,
            pool: true,
        });
    clean.chain(pools).collect()
}

/// What one group's public worlds need (W13, WORLDS_OFFICIAL.md §13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpareStep {
    /// No warm spare: open one.
    Open,
    /// More than one warm spare: close this extra one (an index into the group's list).
    Close(usize),
    Keep,
}

/// A group's step from its worlds, oldest first, each `(warm, idle_long)`: a warm spare is an
/// empty world (no peer, nobody on the way in, not a Road Trip past its vote), the one the server
/// promotes for a private create or a car meet session and fills last (its quick join seats the
/// oldest world with a free seat). So a group always keeps exactly one empty world besides its
/// partly filled ones; an extra empty one closes once idle long enough, newest first.
pub fn spare_step(worlds: &[(bool, bool)]) -> SpareStep {
    let warm = worlds.iter().filter(|(w, _)| *w).count();
    if warm == 0 {
        return SpareStep::Open;
    }
    if warm > 1
        && let Some(i) = worlds.iter().rposition(|&(w, idle)| w && idle)
    {
        return SpareStep::Close(i);
    }
    SpareStep::Keep
}
/// The pools an official world may serve (`OfficialWorldSettings.mode`).
pub const MODES: [&str; 2] = ["hardcore", "noob"];
/// `OfficialWorldSettings.mode` of a mixed world: both modes (MATCHMAKING_API §2.11, CR-S101).
pub const ANY_MODE: &str = "any";

/// The `mode` an activity's spare is opened with (OD-210, CR-S101 §2.11, W14): `"any"` (a mixed
/// world) when the agent serves every mode, else its one mode; the car meet lobby keeps the first
/// mode, as before (it admits both modes by its activity, OD-180).
pub fn spare_mode(modes: &[&'static str], activity: &str) -> &'static str {
    let first = modes.first().copied().unwrap_or("noob");
    if activity == MIXED {
        first
    } else if MODES.iter().all(|m| modes.contains(m)) {
        ANY_MODE
    } else {
        first
    }
}

/// The placement groups, each kept with one spare (OD-210, CR-S101, W14): one per (game,
/// activity) placed, with [`spare_mode`]'s mode. Free roam first, the car meet lobby, then Road
/// Trip: the order spares are restored in; FM5's `race` after FH2's (CR-S99 §5), within the same
/// `maxWorlds`.
pub fn spare_groups(
    modes: &[&'static str],
    activities: &[&'static str],
) -> Vec<(&'static str, &'static str)> {
    if modes.is_empty() {
        return Vec::new();
    }
    ALL_ACTIVITIES
        .into_iter()
        .filter(|a| activities.contains(a))
        .map(|a| (spare_mode(modes, a), a))
        .collect()
}
/// FH2, the game of the activities above.
pub const GAME: &str = "fh2";
/// FM5's official activity (CR-S99 §1, §5; OD-206): a private race lobby, one mixed spare
/// (OD-210; FM5 is Noob only until M11, so its lobbies are Noob in practice).
pub const FM5_ACTIVITIES: [&str; 1] = ["race"];
pub const FM5: &str = "fm5";
/// Every activity this agent may place, FH2's then FM5's: the order spares are restored in.
pub const ALL_ACTIVITIES: [&str; 4] = ["freeroam", "carmeet", "roadtrip", "race"];
/// FM5's `maxPlayers` default and ceiling (CR-S99 §1: 16 seats).
pub const FM5_MAX_PLAYERS: u64 = 16;

/// The game an activity's world runs: `race` is FM5's (CR-S99 §5), the others FH2's.
pub fn game_of(activity: &str) -> &'static str {
    if FM5_ACTIVITIES.contains(&activity) {
        FM5
    } else {
        GAME
    }
}

/// The server's base URL, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerUrl {
    /// `scheme://authority`, no trailing slash: request URLs are `base + target`.
    pub base: String,
    pub https: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentConfig {
    pub server: ServerUrl,
    pub vm_id: String,
    pub key_file: PathBuf,
    /// The engine's public UDP socket (`worlds serve --bind`).
    pub bind: SocketAddr,
    /// The engine's control socket (`unix:<path>`).
    pub control: PathBuf,
    pub catalog: Option<PathBuf>,
    /// `None`: the hello's `activities`.
    pub activities: Option<Vec<&'static str>>,
    pub modes: Vec<&'static str>,
    /// The most worlds this VM keeps open (the engine's `--max-worlds`, 1-32, default 20).
    pub max_worlds: usize,
    /// The engine's `--memory-limit-mb` (128-65536; default `max_worlds` FH2 worlds' §8
    /// reservations plus the 20 MB headroom): at most what the engine's container may use.
    pub memory_limit_mb: u64,
    /// Keep pool spares beside the clean ones (WORLDS_OFFICIAL.md §14; `1` default, `0` off).
    pub pool_spares: bool,
    /// Each FH2 world's `maxPlayers` (guests, 2-12).
    pub max_players: u8,
    /// Each FM5 world's `maxPlayers` (2-16, default 16; CR-S99 §5).
    pub fm5_max_players: u8,
    /// Seconds between one world's announces (1-15; 15 in production).
    pub announce_seconds: u64,
}

const OPTIONS: &[(&str, &str)] = &[
    ("--server-url", "FI_WORLDS_SERVER_URL"),
    ("--vm-id", "FI_WORLDS_VM_ID"),
    ("--key-file", "FI_WORLDS_KEY_FILE"),
    ("--bind", "FI_WORLDS_BIND"),
    ("--control", "FI_WORLDS_CONTROL"),
    ("--catalog", "FI_WORLDS_CATALOG"),
    ("--activities", "FI_WORLDS_ACTIVITIES"),
    ("--modes", "FI_WORLDS_MODES"),
    ("--max-worlds", "FI_WORLDS_MAX_WORLDS"),
    ("--max-players", "FI_WORLDS_MAX_PLAYERS"),
    ("--fm5-max-players", "FI_WORLDS_FM5_MAX_PLAYERS"),
    ("--announce-seconds", "FI_WORLDS_ANNOUNCE_SECONDS"),
    ("--memory-limit-mb", "FI_WORLDS_MEMORY_LIMIT_MB"),
    ("--pool-spares", "FI_WORLDS_POOL_SPARES"),
];

/// A world's §8 reservation: its base plus its game's seat share × `maxPlayers` (the engine's
/// own rule, `core::server`).
pub fn world_reserve_mb(game: &str, max_players: u8) -> u64 {
    let seat = if game == GAME {
        crate::fh2::SEAT_RESERVE_MB
    } else {
        crate::core::server::SEAT_MB
    };
    crate::core::server::WORLD_BASE_MB + seat * u64::from(max_players)
}

/// The default control socket (DEDICATED_SERVER.md §3.1).
pub const DEFAULT_CONTROL: &str = "/run/fi-ds/control.sock";

pub fn parse(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<AgentConfig, Invalid> {
    let mut given: Vec<(&'static str, String)> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        let (_, var) = OPTIONS.iter().find(|(o, _)| *o == name).ok_or("argument")?;
        if given.iter().any(|(g, _)| g == var) {
            return Err(var);
        }
        let value = match inline {
            Some(v) => v,
            None => it.next().cloned().ok_or(*var)?,
        };
        given.push((var, value));
    }
    let value = |var: &'static str| -> Option<String> {
        given
            .iter()
            .find(|(g, _)| *g == var)
            .map(|(_, v)| v.clone())
            .or_else(|| env(var))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let int = |var: &'static str, default: u64, ok: &dyn Fn(u64) -> bool| -> Result<u64, Invalid> {
        match value(var) {
            None => Ok(default),
            Some(t) => match t.parse::<u64>() {
                Ok(n) if ok(n) && (t == "0" || !t.starts_with('0')) => Ok(n),
                _ => Err(var),
            },
        }
    };

    let server = server_url(&value("FI_WORLDS_SERVER_URL").ok_or("FI_WORLDS_SERVER_URL")?)
        .ok_or("FI_WORLDS_SERVER_URL")?;
    let vm_id = value("FI_WORLDS_VM_ID").ok_or("FI_WORLDS_VM_ID")?;
    if !crate::core::grant::vm_id(&vm_id) {
        return Err("FI_WORLDS_VM_ID");
    }
    let key_file = PathBuf::from(value("FI_WORLDS_KEY_FILE").ok_or("FI_WORLDS_KEY_FILE")?);
    if !key_file.is_absolute() || key_file.file_name().is_none() {
        return Err("FI_WORLDS_KEY_FILE");
    }
    let bind = value("FI_WORLDS_BIND")
        .ok_or("FI_WORLDS_BIND")?
        .parse::<SocketAddr>()
        .map_err(|_| "FI_WORLDS_BIND")?;
    let control = match value("FI_WORLDS_CONTROL") {
        None => PathBuf::from(DEFAULT_CONTROL),
        Some(t) => {
            let path = t.strip_prefix("unix:").unwrap_or(&t);
            // the engine's own rule for `unix:<path>` (DEDICATED_SERVER.md §2)
            let p = PathBuf::from(path);
            if !p.is_absolute() || path.len() > 100 || p.file_name().is_none() {
                return Err("FI_WORLDS_CONTROL");
            }
            p
        }
    };
    let catalog = match value("FI_WORLDS_CATALOG") {
        None => None,
        Some(t) => {
            let p = PathBuf::from(t);
            if !p.is_absolute() || !p.is_file() {
                return Err("FI_WORLDS_CATALOG");
            }
            Some(p)
        }
    };
    let activities = match value("FI_WORLDS_ACTIVITIES") {
        None => None,
        Some(t) => Some(list(&t, &ALL_ACTIVITIES).ok_or("FI_WORLDS_ACTIVITIES")?),
    };
    let modes = match value("FI_WORLDS_MODES") {
        None => MODES.to_vec(),
        Some(t) => list(&t, &MODES).ok_or("FI_WORLDS_MODES")?,
    };
    let max_worlds = int("FI_WORLDS_MAX_WORLDS", DEFAULT_MAX_WORLDS, &|n| {
        (1..=crate::cli::PUBLIC_MAX_WORLDS).contains(&n)
    })? as usize;
    let max_players = int("FI_WORLDS_MAX_PLAYERS", 12, &|n| (2..=12).contains(&n))? as u8;
    let fm5_max_players = int("FI_WORLDS_FM5_MAX_PLAYERS", FM5_MAX_PLAYERS, &|n| {
        (2..=FM5_MAX_PLAYERS).contains(&n)
    })? as u8;
    let announce_seconds = int("FI_WORLDS_ANNOUNCE_SECONDS", 15, &|n| (1..=15).contains(&n))?;
    let memory_default = max_worlds as u64 * world_reserve_mb(GAME, max_players)
        + crate::core::server::BUDGET_HEADROOM_MB;
    // the engine's own range (`--memory-limit-mb`, DEDICATED_SERVER.md §2)
    let memory_limit_mb = int("FI_WORLDS_MEMORY_LIMIT_MB", memory_default.max(128), &|n| {
        (128..=65_536).contains(&n)
    })?;
    let pool_spares = int("FI_WORLDS_POOL_SPARES", 1, &|n| n <= 1)? == 1;
    Ok(AgentConfig {
        server,
        vm_id,
        key_file,
        bind,
        control,
        catalog,
        activities,
        modes,
        max_worlds,
        memory_limit_mb,
        pool_spares,
        max_players,
        fm5_max_players,
        announce_seconds,
    })
}

impl AgentConfig {
    /// The engine's `--max-seats` for these placed activities: every world at the largest
    /// `maxPlayers` among them (FH2's when none seats more).
    pub fn engine_seats(&self, activities: &[&str]) -> usize {
        let per_world = activities
            .iter()
            .map(|a| self.max_players_of(a))
            .fold(self.max_players, u8::max);
        self.max_worlds * usize::from(per_world)
    }

    /// The `maxPlayers` of an activity's worlds (its game's setting).
    pub fn max_players_of(&self, activity: &str) -> u8 {
        if game_of(activity) == FM5 {
            self.fm5_max_players
        } else {
            self.max_players
        }
    }
}

/// A comma list of distinct names out of `allowed`, at least one.
fn list(text: &str, allowed: &[&'static str]) -> Option<Vec<&'static str>> {
    let mut out: Vec<&'static str> = Vec::new();
    for item in text.split(',') {
        let item = item.trim();
        let name = allowed.iter().find(|a| **a == item)?;
        if out.contains(name) {
            return None;
        }
        out.push(name);
    }
    (!out.is_empty()).then_some(out)
}

/// `https://host[:port]` (any host), or `http://` only for a loopback or RFC 1918 IP literal or
/// `localhost` (a development stack, WORLDS_OFFICIAL.md §2). No user info, path (but `/`), query
/// or fragment: the signed target is the request's own path.
pub fn server_url(text: &str) -> Option<ServerUrl> {
    let (https, rest) = match text.strip_prefix("https://") {
        Some(r) => (true, r),
        None => (false, text.strip_prefix("http://")?),
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.is_empty()
        || authority
            .bytes()
            .any(|b| matches!(b, b'/' | b'?' | b'#' | b'@' | b'\\') || !b.is_ascii_graphic())
    {
        return None;
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?),
        };
        (h, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        }
    };
    if let Some(p) = port {
        match p.parse::<u16>() {
            Ok(n) if n != 0 && !p.starts_with('0') => {}
            _ => return None,
        }
    }
    let ip = host.parse::<IpAddr>().ok();
    if authority.starts_with('[') && !matches!(ip, Some(IpAddr::V6(_))) {
        return None;
    }
    if ip.is_none()
        && (host.is_empty()
            || host.contains(':')
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.'))
    {
        return None;
    }
    if !https {
        let local = match ip {
            Some(IpAddr::V4(v4)) => v4.is_loopback() || rfc1918(v4),
            Some(IpAddr::V6(v6)) => v6.is_loopback(),
            None => host.eq_ignore_ascii_case("localhost"),
        };
        if !local {
            return None;
        }
    }
    Some(ServerUrl {
        base: format!("{}://{authority}", if https { "https" } else { "http" }),
        https,
    })
}

fn rfc1918(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 10 || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168)
}

/// Why the node key could not be had.
#[derive(Debug)]
pub enum KeyError {
    /// The file exists but is not standard base64 of 32 bytes.
    Malformed,
    Io(std::io::Error),
    Random,
}

/// Reads the node key (standard base64 of the 32-byte Ed25519 seed, one line), or creates it once
/// (0600, written to a partial file and hard-linked into place, so two starts never get two
/// identities). Returns the key and whether this call created it.
pub fn load_or_create_key(path: &Path) -> Result<(SigningKey, bool), KeyError> {
    if path.exists() {
        return read_key(path).map(|k| (k, false));
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|_| KeyError::Random)?;
    let mut tag = [0u8; 6];
    getrandom::fill(&mut tag).map_err(|_| KeyError::Random)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let partial = path.with_file_name(format!(".{name}.{}.partial", super::sign::hex(&tag)));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| {
        use std::io::Write;
        let mut file = options.open(&partial)?;
        file.write_all(encode_base64(&seed).as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::hard_link(&partial, path)
    })();
    let _ = std::fs::remove_file(&partial);
    match written {
        Ok(()) => Ok((SigningKey::from_bytes(&seed), true)),
        // another start wrote it meanwhile: theirs is the identity
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            read_key(path).map(|k| (k, false))
        }
        Err(e) => Err(KeyError::Io(e)),
    }
}

fn read_key(path: &Path) -> Result<SigningKey, KeyError> {
    let text = std::fs::read_to_string(path).map_err(KeyError::Io)?;
    let raw = decode_base64(text.trim()).ok_or(KeyError::Malformed)?;
    let seed: [u8; 32] = raw.try_into().map_err(|_| KeyError::Malformed)?;
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<(&'static str, String)> {
        vec![
            ("FI_WORLDS_SERVER_URL", "https://mm.example.test".into()),
            ("FI_WORLDS_VM_ID", "sao-1".into()),
            ("FI_WORLDS_KEY_FILE", "/var/lib/fi-worlds/agent.key".into()),
            ("FI_WORLDS_BIND", "[::]:5101".into()),
        ]
    }

    fn with(over: &[(&'static str, &str)], args: &[&str]) -> Result<AgentConfig, Invalid> {
        let mut vars = base();
        for (k, v) in over {
            vars.retain(|(n, _)| n != k);
            vars.push((k, v.to_string()));
        }
        let env = move |name: &str| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone())
        };
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&args, &env)
    }

    // The fixtures' key and catalog paths are Unix absolute paths (the role runs on Unix only).
    #[cfg(unix)]
    #[test]
    fn the_defaults_and_the_precedence() {
        let c = with(&[], &[]).unwrap();
        assert_eq!(c.server.base, "https://mm.example.test");
        assert_eq!(c.vm_id, "sao-1");
        assert_eq!(c.bind, "[::]:5101".parse().unwrap());
        assert_eq!(c.control, PathBuf::from(DEFAULT_CONTROL));
        assert_eq!(c.catalog, None);
        assert_eq!(c.activities, None);
        assert_eq!(c.modes, vec!["hardcore", "noob"]);
        assert_eq!(
            (c.max_worlds, c.max_players, c.announce_seconds),
            (20, 12, 15)
        );
        assert_eq!((c.memory_limit_mb, c.pool_spares), (580, true));
        let c = with(
            &[("FI_WORLDS_VM_ID", "env-vm"), ("FI_WORLDS_MODES", "noob")],
            &["--vm-id", "cli-vm", "--activities=roadtrip,freeroam"],
        )
        .unwrap();
        assert_eq!(c.vm_id, "cli-vm");
        assert_eq!(c.modes, vec!["noob"]);
        assert_eq!(c.activities, Some(vec!["roadtrip", "freeroam"]));
        // OD-190: the car meet lobby is an activity of its own
        let c = with(&[("FI_WORLDS_ACTIVITIES", "carmeet,freeroam")], &[]).unwrap();
        assert_eq!(c.activities, Some(vec!["carmeet", "freeroam"]));
    }

    // The fixtures' key and catalog paths are Unix absolute paths (the role runs on Unix only).
    #[cfg(unix)]
    #[test]
    fn every_invalid_setting_is_named() {
        for (var, bad) in [
            ("FI_WORLDS_SERVER_URL", "ftp://x"),
            ("FI_WORLDS_SERVER_URL", "http://mm.example.test"),
            ("FI_WORLDS_SERVER_URL", "http://8.8.8.8:8480"),
            ("FI_WORLDS_SERVER_URL", "http://172.32.0.1"),
            ("FI_WORLDS_SERVER_URL", "https://mm.example.test/base"),
            ("FI_WORLDS_SERVER_URL", "https://u:p@mm.example.test"),
            ("FI_WORLDS_SERVER_URL", "https://mm.example.test?x"),
            ("FI_WORLDS_SERVER_URL", "https://mm.example.test:0"),
            ("FI_WORLDS_SERVER_URL", "https://mm.example.test:99999"),
            ("FI_WORLDS_SERVER_URL", "https://[::1"),
            ("FI_WORLDS_SERVER_URL", "https://"),
            ("FI_WORLDS_VM_ID", "Sao-1"),
            ("FI_WORLDS_VM_ID", "a_b"),
            ("FI_WORLDS_VM_ID", "abcdefghijklmnopqrstuvwxyz0123456"),
            ("FI_WORLDS_KEY_FILE", "relative.key"),
            ("FI_WORLDS_BIND", "5101"),
            ("FI_WORLDS_BIND", "host:5101"),
            ("FI_WORLDS_CONTROL", "relative.sock"),
            ("FI_WORLDS_CATALOG", "/nonexistent/gamedbRC.slt"),
            ("FI_WORLDS_ACTIVITIES", "carmeet3"),
            ("FI_WORLDS_ACTIVITIES", "carmeet,carmeet"),
            ("FI_WORLDS_ACTIVITIES", "freeroam,freeroam"),
            ("FI_WORLDS_ACTIVITIES", ","),
            ("FI_WORLDS_MODES", "any"),
            ("FI_WORLDS_MAX_WORLDS", "0"),
            ("FI_WORLDS_MAX_WORLDS", "33"),
            ("FI_WORLDS_MAX_WORLDS", "04"),
            ("FI_WORLDS_MEMORY_LIMIT_MB", "127"),
            ("FI_WORLDS_MEMORY_LIMIT_MB", "65537"),
            ("FI_WORLDS_MEMORY_LIMIT_MB", "0580"),
            ("FI_WORLDS_POOL_SPARES", "2"),
            ("FI_WORLDS_POOL_SPARES", "true"),
            ("FI_WORLDS_POOL_SPARES", "01"),
            ("FI_WORLDS_MAX_PLAYERS", "1"),
            ("FI_WORLDS_MAX_PLAYERS", "13"),
            ("FI_WORLDS_ANNOUNCE_SECONDS", "16"),
            ("FI_WORLDS_ANNOUNCE_SECONDS", "0"),
        ] {
            assert_eq!(with(&[(var, bad)], &[]), Err(var), "{var}={bad}");
        }
        for missing in [
            "FI_WORLDS_SERVER_URL",
            "FI_WORLDS_VM_ID",
            "FI_WORLDS_KEY_FILE",
            "FI_WORLDS_BIND",
        ] {
            assert_eq!(with(&[(missing, " ")], &[]), Err(missing));
        }
        assert_eq!(with(&[], &["--nope", "x"]), Err("argument"));
        assert_eq!(with(&[], &["--bind"]), Err("FI_WORLDS_BIND"));
        assert_eq!(
            with(&[], &["--bind", "[::]:1", "--bind=[::]:2"]),
            Err("FI_WORLDS_BIND")
        );
    }

    /// T2 (SAURON, 2026-11-07): the engine refused `--max-seats` 128 and restarted forever. Every
    /// placement and setting the agent allows gives seats the engine's public profile accepts;
    /// FH2's default is 20 × 12 = 240 (W13).
    #[test]
    fn the_engines_seats_fit_its_public_bound_for_every_placement() {
        let key = if cfg!(windows) {
            "C:\\fi-worlds\\agent.key"
        } else {
            "/var/lib/fi-worlds/agent.key"
        };
        let base = with(&[("FI_WORLDS_KEY_FILE", key)], &[]).unwrap();
        assert_eq!(base.engine_seats(&ACTIVITIES), 240);
        assert_eq!(base.engine_seats(&ALL_ACTIVITIES), 320);
        assert_eq!(base.engine_seats(&FM5_ACTIVITIES), 320);
        let placements: [&[&str]; 4] = [&ACTIVITIES, &FM5_ACTIVITIES, &ALL_ACTIVITIES, &[]];
        for worlds in 1..=crate::cli::PUBLIC_MAX_WORLDS as usize {
            for fh2 in 2..=12u8 {
                for fm5 in 2..=16u8 {
                    let c = AgentConfig {
                        max_worlds: worlds,
                        max_players: fh2,
                        fm5_max_players: fm5,
                        ..base.clone()
                    };
                    for placed in placements {
                        let seats = c.engine_seats(placed) as u64;
                        assert!(
                            (1..=crate::cli::PUBLIC_MAX_SEATS).contains(&seats),
                            "{worlds} worlds, {fh2}/{fm5} seats, {placed:?}: {seats}"
                        );
                    }
                }
            }
        }
    }

    /// The agent's exact engine arguments pass the engine's own parser (the control socket is a
    /// Unix one, so this runs on Linux).
    #[cfg(unix)]
    #[test]
    fn the_engines_arguments_parse() {
        let base = with(
            &[("FI_WORLDS_KEY_FILE", "/var/lib/fi-worlds/agent.key")],
            &[],
        )
        .unwrap();
        for placed in [&ACTIVITIES[..], &ALL_ACTIVITIES[..], &FM5_ACTIVITIES[..]] {
            let args = super::super::engine::serve_args(
                "0.0.0.0:5101",
                std::path::Path::new("/run/fi-ds/control.sock"),
                base.max_worlds,
                base.engine_seats(placed),
                base.memory_limit_mb,
            );
            let Ok(crate::cli::Command::Run(rc)) = crate::cli::parse(&args[1..], &|_| None) else {
                panic!("{placed:?}: {args:?}");
            };
            assert_eq!(
                (rc.max_worlds, rc.max_seats, rc.memory_limit_mb),
                (base.max_worlds, base.engine_seats(placed), 580)
            );
        }
        // the largest setting the agent accepts still parses
        let max = with(
            &[
                ("FI_WORLDS_KEY_FILE", "/var/lib/fi-worlds/agent.key"),
                ("FI_WORLDS_MAX_WORLDS", "32"),
            ],
            &[],
        )
        .unwrap();
        let args = super::super::engine::serve_args(
            "0.0.0.0:5101",
            std::path::Path::new("/run/fi-ds/control.sock"),
            max.max_worlds,
            max.engine_seats(&ALL_ACTIVITIES),
            max.memory_limit_mb,
        );
        assert!(crate::cli::parse(&args[1..], &|_| None).is_ok(), "{args:?}");
    }

    /// W13 (OD-218): the default ceiling's §8 budget. 20 FH2 worlds at 12 seats reserve 28 MB
    /// each (4 + 2 × 12), 560 MB plus the 20 MB headroom; FM5's 16-seat worlds 52 MB each.
    #[test]
    fn the_memory_limit_follows_the_ceiling_unless_set() {
        let key = if cfg!(windows) {
            "C:\\fi-worlds\\agent.key"
        } else {
            "/var/lib/fi-worlds/agent.key"
        };
        assert_eq!(world_reserve_mb(GAME, 12), 28);
        assert_eq!(world_reserve_mb(FM5, 16), 52);
        let c = with(&[("FI_WORLDS_KEY_FILE", key)], &[]).unwrap();
        assert_eq!((c.max_worlds, c.memory_limit_mb), (20, 580));
        let c = with(
            &[
                ("FI_WORLDS_KEY_FILE", key),
                ("FI_WORLDS_MAX_WORLDS", "16"),
                ("FI_WORLDS_MAX_PLAYERS", "8"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(c.memory_limit_mb, 16 * 20 + 20);
        // a small ceiling still gives the engine its smallest limit
        let c = with(
            &[("FI_WORLDS_KEY_FILE", key), ("FI_WORLDS_MAX_WORLDS", "1")],
            &[],
        )
        .unwrap();
        assert_eq!(c.memory_limit_mb, 128);
        let c = with(
            &[("FI_WORLDS_KEY_FILE", key)],
            &["--memory-limit-mb", "600", "--pool-spares=0"],
        )
        .unwrap();
        assert_eq!((c.memory_limit_mb, c.pool_spares), (600, false));
    }

    /// W17 (WORLDS_OFFICIAL.md §14): a pool spare per FH2 group beside each clean one, clean
    /// first; FM5's race has none; `pool` off keeps the clean ones only.
    #[test]
    fn pool_spares_are_fh2s_and_come_after_the_clean_ones() {
        let s = spares(&MODES, &ALL_ACTIVITIES, true);
        let names: Vec<(&str, &str, bool)> =
            s.iter().map(|g| (g.mode, g.activity, g.pool)).collect();
        assert_eq!(
            names,
            vec![
                ("any", "freeroam", false),
                ("hardcore", "carmeet", false),
                ("any", "roadtrip", false),
                ("any", "race", false),
                ("any", "freeroam", true),
                ("hardcore", "carmeet", true),
                ("any", "roadtrip", true),
            ]
        );
        assert!(s.iter().all(|g| !g.pool || game_of(g.activity) == GAME));
        assert_eq!(
            spares(&MODES, &ALL_ACTIVITIES, false),
            spare_groups(&MODES, &ALL_ACTIVITIES)
                .into_iter()
                .map(|(mode, activity)| Spare {
                    mode,
                    activity,
                    pool: false
                })
                .collect::<Vec<_>>()
        );
        assert!(
            spares(&MODES, &FM5_ACTIVITIES, true)
                .iter()
                .all(|g| !g.pool)
        );
        // every spare of the default placement fits the default ceiling with room for the
        // players' worlds (at least 9 full FH2 worlds for 100 players, OD-218)
        assert!(s.len() + 9 < DEFAULT_MAX_WORLDS as usize);
    }

    /// W13 (WORLDS_OFFICIAL.md §13): one empty world per group, whatever its partly filled ones;
    /// an extra empty one closes once idle, newest first; a busy or young one stays.
    #[test]
    fn a_group_keeps_exactly_one_warm_spare() {
        use SpareStep::*;
        assert_eq!(spare_step(&[]), Open);
        // partly filled worlds are no spare: a private create needs an empty one
        assert_eq!(spare_step(&[(false, false), (false, false)]), Open);
        assert_eq!(spare_step(&[(false, false), (true, false)]), Keep);
        assert_eq!(spare_step(&[(true, true)]), Keep);
        assert_eq!(
            spare_step(&[(true, true), (false, false), (true, true)]),
            Close(2)
        );
        assert_eq!(spare_step(&[(true, true), (true, false)]), Close(0));
        assert_eq!(spare_step(&[(true, false), (true, false)]), Keep);
    }

    /// CR-S99 §5: `race` is FM5's activity, its worlds seat up to 16; FH2's are unchanged.
    #[test]
    fn race_is_fm5s_activity_with_its_own_seats() {
        assert_eq!(game_of("race"), "fm5");
        for a in ACTIVITIES {
            assert_eq!(game_of(a), "fh2", "{a}");
        }
        let key = if cfg!(windows) {
            "C:\\fi-worlds\\agent.key"
        } else {
            "/var/lib/fi-worlds/agent.key"
        };
        let c = with(
            &[("FI_WORLDS_KEY_FILE", key)],
            &["--activities", "race,freeroam"],
        )
        .unwrap();
        assert_eq!(c.activities, Some(vec!["race", "freeroam"]));
        assert_eq!((c.max_players, c.fm5_max_players), (12, 16));
        assert_eq!(
            (c.max_players_of("freeroam"), c.max_players_of("race")),
            (12, 16)
        );
        let c = with(
            &[
                ("FI_WORLDS_KEY_FILE", key),
                ("FI_WORLDS_FM5_MAX_PLAYERS", "8"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(c.max_players_of("race"), 8);
        for bad in ["1", "17", "016"] {
            assert_eq!(
                with(
                    &[
                        ("FI_WORLDS_KEY_FILE", key),
                        ("FI_WORLDS_FM5_MAX_PLAYERS", bad)
                    ],
                    &[]
                ),
                Err("FI_WORLDS_FM5_MAX_PLAYERS"),
                "{bad}"
            );
        }
        assert_eq!(
            with(
                &[
                    ("FI_WORLDS_KEY_FILE", key),
                    ("FI_WORLDS_ACTIVITIES", "race,race")
                ],
                &[]
            ),
            Err("FI_WORLDS_ACTIVITIES")
        );
    }

    /// OD-210, CR-S101 (W14): one mixed (`"any"`) spare per (game, activity) instead of one per
    /// mode; the car meet lobby keeps the agent's first mode as before.
    #[test]
    fn one_mixed_spare_per_game_and_activity_and_the_car_meet_as_before() {
        let fh2 = spare_groups(&MODES, &ACTIVITIES);
        assert_eq!(
            fh2,
            vec![
                ("any", "freeroam"),
                ("hardcore", "carmeet"),
                ("any", "roadtrip"),
            ]
        );
        let all = spare_groups(&MODES, &ALL_ACTIVITIES);
        assert_eq!(
            all,
            vec![
                ("any", "freeroam"),
                ("hardcore", "carmeet"),
                ("any", "roadtrip"),
                ("any", "race"),
            ]
        );
        // every placed spare fits the default cap, with room for promoted private worlds
        assert!(all.len() < DEFAULT_MAX_WORLDS as usize);
        // the order of the list does not matter to "any"; the car meet takes the first mode
        assert_eq!(
            spare_groups(&["noob", "hardcore"], &["carmeet", "freeroam"]),
            vec![("any", "freeroam"), ("noob", "carmeet")]
        );
        // an agent serving one mode keeps single-mode spares of it (the server accepts both)
        assert_eq!(
            spare_groups(&["noob"], &["roadtrip", "freeroam", "carmeet", "race"]),
            vec![
                ("noob", "freeroam"),
                ("noob", "carmeet"),
                ("noob", "roadtrip"),
                ("noob", "race"),
            ]
        );
        assert!(spare_groups(&MODES, &[]).is_empty());
        assert!(spare_groups(&[], &ACTIVITIES).is_empty());
        assert_eq!(spare_mode(&MODES, "freeroam"), ANY_MODE);
        assert_eq!(spare_mode(&["hardcore"], "race"), "hardcore");
        // "any" is a world's mode, never a pool the agent is configured to serve
        assert!(!MODES.contains(&ANY_MODE));
    }

    #[test]
    fn http_only_for_a_local_development_server() {
        for ok in [
            "http://127.0.0.1:8480",
            "http://localhost:8480/",
            "http://[::1]:8480",
            "http://10.1.2.3",
            "http://172.16.0.1:80",
            "http://172.31.255.255",
            "http://192.168.68.53:8480",
            "https://mm.example.test:443",
            "https://[2001:db8::1]:8443",
            "https://203.0.113.7",
        ] {
            assert!(server_url(ok).is_some(), "{ok}");
        }
        assert_eq!(
            server_url("http://192.168.68.53:8480/").unwrap(),
            ServerUrl {
                base: "http://192.168.68.53:8480".into(),
                https: false
            }
        );
        for bad in [
            "http://192.169.0.1",
            "http://[fd00::1]",
            "http://example.test",
            "https://exa mple.test",
            "https://[::1]x",
            "https://mm.example.test#f",
        ] {
            assert!(server_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_node_key_is_created_once_0600_and_read_back() {
        let dir = std::env::temp_dir().join(format!("fi-agent-key-{}", super::super::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent.key");
        let (a, created) = load_or_create_key(&path).unwrap();
        assert!(created);
        let (b, created) = load_or_create_key(&path).unwrap();
        assert!(!created);
        assert_eq!(a.to_bytes(), b.to_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim().len(), 44);
        std::fs::write(&path, "not base64\n").unwrap();
        assert!(matches!(
            load_or_create_key(&path),
            Err(KeyError::Malformed)
        ));
        std::fs::write(&path, encode_base64(&[1u8; 31])).unwrap();
        assert!(matches!(
            load_or_create_key(&path),
            Err(KeyError::Malformed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
