// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! The command line (contract §2): options, their `FI_DS_*` environment twins and defaults
//! (precedence CLI, then environment, then defaults), and the validation that makes a bad value
//! exit 2 with one `ds.config.invalid` line naming the option.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use crate::{core::Profile, core::keys::KeyRing, log::Level};

/// The control endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// `unix:<absolute path>` (Linux).
    Unix(PathBuf),
    /// `pipe:\\.\pipe\fi-ds-<32 hex>` (Windows, Proton).
    Pipe(String),
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub profile: Profile,
    pub control: Endpoint,
    pub secret: Option<String>,
    pub game_db: Option<PathBuf>,
    pub grant_keys: Option<KeyRing>,
    /// Public profile only: the one public UDP socket every world shares (contract §5.2).
    pub bind: Option<SocketAddr>,
    pub max_worlds: usize,
    pub max_seats: usize,
    pub memory_limit_mb: u64,
    pub drain_seconds: u64,
    pub stats_seconds: u64,
    pub log_level: Level,
}

#[derive(Debug, Clone)]
pub enum Command {
    Version,
    ProbeBind(u16),
    Health(Endpoint),
    Run(Box<RunConfig>),
}

/// The option's name in `ds.config.invalid {field}`.
pub type Invalid = &'static str;

const OPTIONS: &[(&str, &str)] = &[
    ("--profile", "PROFILE"),
    ("--control", "CONTROL"),
    ("--game-db", "GAME_DB"),
    ("--grant-keys", "GRANT_VERIFY_KEYS"),
    ("--bind", "BIND"),
    ("--max-worlds", "MAX_WORLDS"),
    ("--max-seats", "MAX_SEATS"),
    ("--memory-limit-mb", "MEMORY_LIMIT_MB"),
    ("--drain-seconds", "DRAIN_SECONDS"),
    ("--stats-seconds", "STATS_SECONDS"),
    ("--log-level", "LOG_LEVEL"),
];

/// The role named by the first argument (WORLDS_OFFICIAL.md §1: one binary, one role per process).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// `worlds serve <arguments>`: the world engine; the arguments go to [`parse`].
    Serve(Vec<String>),
    /// `worlds --version` (the same line as `worlds serve --version`).
    Version,
    /// `worlds agent <arguments>`: the world agent (WORLDS_OFFICIAL.md §4; DEDICATED_SERVER.md §16);
    /// the arguments go to `agent::config::parse`.
    Agent(Vec<String>),
    /// `worlds relay`: named, not implemented yet (exit 7).
    Unimplemented(&'static str),
}

/// Splits off the role. No role, or an unknown one, is invalid (`field: "role"`, exit 2).
pub fn parse_role(args: &[String]) -> Result<Role, Invalid> {
    match args.split_first() {
        Some((role, rest)) if role == "serve" => Ok(Role::Serve(rest.to_vec())),
        Some((flag, [])) if flag == "--version" => Ok(Role::Version),
        Some((role, _)) if role == "relay" => Ok(Role::Unimplemented("relay")),
        Some((role, rest)) if role == "agent" => Ok(Role::Agent(rest.to_vec())),
        _ => Err("role"),
    }
}

/// The environment variable of the secret (read once, then removed by `main`).
pub const SECRET_ENV: &str = "FI_DS_CONTROL_SECRET";

pub fn parse(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> Result<Command, Invalid> {
    let mut given: Vec<(&'static str, String)> = Vec::new();
    let mut version = false;
    let mut health = false;
    let mut probe: Option<String> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        match name {
            "--version" if inline.is_none() => version = true,
            "--health" if inline.is_none() => health = true,
            "--probe-bind" => {
                probe = Some(match inline {
                    Some(v) => v,
                    None => it.next().cloned().ok_or("--probe-bind")?,
                })
            }
            _ => {
                let (opt, _) = OPTIONS.iter().find(|(o, _)| *o == name).ok_or("argument")?;
                if given.iter().any(|(g, _)| g == opt) {
                    return Err(opt);
                }
                let value = match inline {
                    Some(v) => v,
                    None => it.next().cloned().ok_or(*opt)?,
                };
                given.push((opt, value));
            }
        }
    }
    if version {
        if health || probe.is_some() || !given.is_empty() {
            return Err("--version");
        }
        return Ok(Command::Version);
    }
    if let Some(p) = probe {
        if health || !given.is_empty() {
            return Err("--probe-bind");
        }
        let port = p
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .ok_or("--probe-bind")?;
        return Ok(Command::ProbeBind(port));
    }
    let cli = |opt: &str| {
        given
            .iter()
            .find(|(g, _)| *g == opt)
            .map(|(_, v)| v.clone())
    };
    let value = |opt: &'static str| -> Option<String> {
        cli(opt).or_else(|| {
            let (_, e) = OPTIONS.iter().find(|(o, _)| *o == opt)?;
            env(&format!("FI_DS_{e}"))
        })
    };
    let control = parse_endpoint(&value("--control").ok_or("--control")?).ok_or("--control")?;
    if health {
        if given.iter().any(|(g, _)| *g != "--control") {
            return Err("--health");
        }
        return match control {
            Endpoint::Unix(_) => Ok(Command::Health(control)),
            // one pipe instance only: the launcher's; there is nothing to probe
            Endpoint::Pipe(_) => Err("--health"),
        };
    }
    let profile = Profile::parse(&value("--profile").ok_or("--profile")?).ok_or("--profile")?;
    let public = profile == Profile::Public;
    let secret = match &control {
        Endpoint::Pipe(_) => {
            let s = env(SECRET_ENV).ok_or("FI_DS_CONTROL_SECRET")?;
            let ok = s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !ok {
                return Err("FI_DS_CONTROL_SECRET");
            }
            Some(s)
        }
        Endpoint::Unix(_) => None,
    };
    let game_db = match value("--game-db") {
        Some(p) if Path::new(&p).is_absolute() => Some(PathBuf::from(p)),
        Some(_) => return Err("--game-db"),
        None => None,
    };
    let grant_keys = match value("--grant-keys") {
        Some(t) => Some(KeyRing::parse(&t).map_err(|_| "--grant-keys")?),
        None => None,
    };
    let int = |opt: &'static str, default: u64, ok: &dyn Fn(u64) -> bool| -> Result<u64, Invalid> {
        match value(opt) {
            None => Ok(default),
            Some(v) => v.parse::<u64>().ok().filter(|n| ok(*n)).ok_or(opt),
        }
    };
    let max_worlds = if public {
        int("--max-worlds", 4, &|n| (1..=8).contains(&n))?
    } else {
        int("--max-worlds", 1, &|n| n == 1)?
    } as usize;
    let max_seats = if public {
        int("--max-seats", 48, &|n| (1..=96).contains(&n))?
    } else {
        int("--max-seats", 12, &|n| (1..=12).contains(&n))?
    } as usize;
    let memory_limit_mb = if public {
        int("--memory-limit-mb", 400, &|n| (128..=65_536).contains(&n))?
    } else {
        int("--memory-limit-mb", 0, &|n| {
            n == 0 || (128..=65_536).contains(&n)
        })?
    };
    let drain_seconds = int("--drain-seconds", 25, &|n| n <= 300)?;
    let stats_seconds = int("--stats-seconds", 60, &|n| {
        n == 0 || (10..=3600).contains(&n)
    })?;
    let log_level = match value("--log-level") {
        None => Level::Info,
        Some(v) => Level::parse(&v).ok_or("--log-level")?,
    };
    if public && log_level == Level::Debug {
        return Err("--log-level");
    }
    // public: the VM's public `addr:port` (the `FI_WORLDS` endpoint), required; private: none
    // (each world binds its own port at `world.open`)
    let bind = match (public, value("--bind")) {
        (true, Some(text)) => Some(text.parse::<SocketAddr>().map_err(|_| "--bind")?),
        (true, None) => return Err("--bind"),
        (false, _) if cli("--bind").is_some() => return Err("--bind"),
        (false, _) => None,
    };
    Ok(Command::Run(Box::new(RunConfig {
        profile,
        control,
        secret,
        game_db,
        grant_keys,
        bind,
        max_worlds,
        max_seats,
        memory_limit_mb,
        drain_seconds,
        stats_seconds,
        log_level,
    })))
}

/// `unix:<absolute path>` on Unix builds, `pipe:\\.\pipe\fi-ds-<32 hex>` on Windows builds.
pub fn parse_endpoint(s: &str) -> Option<Endpoint> {
    if let Some(path) = s.strip_prefix("unix:") {
        let p = PathBuf::from(path);
        return (cfg!(unix) && p.is_absolute() && path.len() <= 100 && p.file_name().is_some())
            .then_some(Endpoint::Unix(p));
    }
    if let Some(name) = s.strip_prefix("pipe:") {
        let ok = name.strip_prefix(r"\\.\pipe\fi-ds-").is_some_and(|hex| {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        return (cfg!(windows) && ok).then(|| Endpoint::Pipe(name.to_string()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_first_argument_names_the_role() {
        assert_eq!(
            parse_role(&args(&["serve", "--profile", "private"])),
            Ok(Role::Serve(args(&["--profile", "private"])))
        );
        assert_eq!(parse_role(&args(&["--version"])), Ok(Role::Version));
        assert_eq!(
            parse_role(&args(&["serve", "--version"])),
            Ok(Role::Serve(args(&["--version"])))
        );
        assert_eq!(
            parse_role(&args(&["relay"])),
            Ok(Role::Unimplemented("relay"))
        );
        assert_eq!(
            parse_role(&args(&["agent", "--x"])),
            Ok(Role::Agent(args(&["--x"])))
        );
        for bad in [
            &[][..],
            &["--profile", "private"],
            &["Serve"],
            &["--version", "x"],
        ] {
            assert_eq!(parse_role(&args(bad)), Err("role"));
        }
    }
}
