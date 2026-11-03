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

/// The activities an official world may run (MATCHMAKING_API §2.9 `OfficialWorldSettings`).
pub const ACTIVITIES: [&str; 2] = ["freeroam", "roadtrip"];
/// The pools an official world may serve (`OfficialWorldSettings.mode`).
pub const MODES: [&str; 2] = ["hardcore", "noob"];
/// The one game module this build hosts.
pub const GAME: &str = "fh2";

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
    /// The most worlds this VM keeps open (the engine's `--max-worlds`, 1-8).
    pub max_worlds: usize,
    /// Each world's `maxPlayers` (guests, 2-12).
    pub max_players: u8,
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
    ("--announce-seconds", "FI_WORLDS_ANNOUNCE_SECONDS"),
];

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
        Some(t) => Some(list(&t, &ACTIVITIES).ok_or("FI_WORLDS_ACTIVITIES")?),
    };
    let modes = match value("FI_WORLDS_MODES") {
        None => MODES.to_vec(),
        Some(t) => list(&t, &MODES).ok_or("FI_WORLDS_MODES")?,
    };
    let max_worlds = int("FI_WORLDS_MAX_WORLDS", 4, &|n| (1..=8).contains(&n))? as usize;
    let max_players = int("FI_WORLDS_MAX_PLAYERS", 12, &|n| (2..=12).contains(&n))? as u8;
    let announce_seconds = int("FI_WORLDS_ANNOUNCE_SECONDS", 15, &|n| (1..=15).contains(&n))?;
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
        max_players,
        announce_seconds,
    })
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
            (4, 12, 15)
        );
        let c = with(
            &[("FI_WORLDS_VM_ID", "env-vm"), ("FI_WORLDS_MODES", "noob")],
            &["--vm-id", "cli-vm", "--activities=roadtrip,freeroam"],
        )
        .unwrap();
        assert_eq!(c.vm_id, "cli-vm");
        assert_eq!(c.modes, vec!["noob"]);
        assert_eq!(c.activities, Some(vec!["roadtrip", "freeroam"]));
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
            ("FI_WORLDS_ACTIVITIES", "carmeet"),
            ("FI_WORLDS_ACTIVITIES", "freeroam,freeroam"),
            ("FI_WORLDS_ACTIVITIES", ","),
            ("FI_WORLDS_MODES", "any"),
            ("FI_WORLDS_MAX_WORLDS", "0"),
            ("FI_WORLDS_MAX_WORLDS", "9"),
            ("FI_WORLDS_MAX_WORLDS", "04"),
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
