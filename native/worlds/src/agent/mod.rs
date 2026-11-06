// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! `worlds agent`: the world agent of a Worlds host (WORLDS_OFFICIAL.md §1, §4, §5;
//! DEDICATED_SERVER.md §16). It is the "host's launcher" of official worlds:
//!
//!   - `config`: the settings (`FI_WORLDS_*` and their options), validated before anything starts;
//!   - `sign`: the `FI-World-Agent` signature of every call (WORLDS_OFFICIAL.md §4.1);
//!   - `private`: a private official world's fields (WORLDS_OFFICIAL.md §12.8b);
//!   - `catalog` (Linux): the server-side game catalog, read-only (`catalog.set`, `catalog.parts`);
//!   - `engine` (Linux): the engine child (`worlds serve --profile public`) and its control channel;
//!   - `http` (Linux): the signed calls to MATCHMAKING_API §2.9;
//!   - `run` (Linux): the supervisor and the placement loop.
//!
//! The Worlds host is a Linux VM, so the I/O half is Unix-only; on Windows `worlds agent` exits 7
//! (`ds.role.unimplemented`), and the signature and the configuration still build and test there.

pub mod config;
pub mod private;
pub mod sign;

#[cfg(unix)]
pub mod catalog;
#[cfg(unix)]
mod engine;
#[cfg(unix)]
mod http;
#[cfg(unix)]
mod run;

/// The agent's exit codes.
pub mod exit {
    /// Stopped by SIGTERM/SIGINT after closing its worlds.
    pub const OK: i32 = 0;
    /// A setting is invalid (`agent.config.invalid {field}`).
    pub const CONFIG: i32 = 2;
    /// Another controller holds the engine's control socket (a second agent on this VM).
    pub const ENGINE_BUSY: i32 = 3;
}

/// Runs the agent with its arguments (after `agent`) and an environment reader; returns the exit
/// code.
#[cfg(unix)]
pub fn main(args: &[String], env: &dyn Fn(&str) -> Option<String>) -> i32 {
    use crate::{log, log_ids};
    let cfg = match config::parse(args, env) {
        Ok(c) => c,
        Err(field) => {
            log::error(log_ids::AGENT_CONFIG_INVALID)
                .field("field", field)
                .emit();
            return exit::CONFIG;
        }
    };
    run::run(cfg)
}

/// A random (version 4) UUID, canonical lowercase: client-minted world ids and idempotency keys.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // no randomness is no identity; the caller's retry mints again
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b = t.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = sign::hex(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn minted_ids_are_canonical_v4_uuids() {
        let a = super::uuid_v4();
        let b = super::uuid_v4();
        assert_ne!(a, b);
        assert!(crate::core::grant::uuid(&a), "{a}");
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "{a}");
    }
}
