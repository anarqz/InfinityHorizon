// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
//! `worlds`: Infinity Worlds, the world engine, one program for private worlds (a child process
//! of the launcher, one world each) and public worlds (a long-lived process on a Worlds host VM).
//!
//! Layers:
//!   - `core`: the deterministic, I/O-free server: control commands, worlds, admission state,
//!     timers, events. Time and randomness come from the caller, datagrams leave through an
//!     output buffer. Golden vectors replay against this layer.
//!   - `core::game`: the trait between the core and a game module (OD-186: one game-agnostic
//!     core, one module per game; `world.open` names the game).
//!   - `fh2`: the Forza Horizon 2 module: NerdCore codec (`codec`, written
//!     from NERDCORE_WIRE.md, OD-187), side channels
//!     (`carstate`), engines and settings. Scaffold only in this step; the game features are
//!     ported later against golden vectors (README).
//!   - `runtime`: the single network loop thread, the sockets, the control transports.
//!   - game-agnostic network pieces: `net` (bind), `stun`, `nat`, `upnp`.
//!
//! The control channel and its rules are the contract `DEDICATED_SERVER.md` (proto 1).

pub mod alloc;
pub mod anon;
pub mod cli;
pub mod control;
pub mod core;
pub mod fh2;
pub mod log;
pub mod log_ids;
pub mod nat;
pub mod net;
pub mod runtime;
pub mod stun;
pub mod upnp;

/// The control protocol versions this build speaks (contract §10).
pub const PROTO_MIN: u32 = 1;
pub const PROTO_MAX: u32 = 1;

/// The game modules this build hosts.
pub fn registered_games(fh2_catalog: fh2::Catalog) -> Vec<std::sync::Arc<dyn core::game::Game>> {
    vec![std::sync::Arc::new(fh2::Fh2::new(fh2_catalog))]
}

/// The build's version (contract §10: `hello.build.version`, compared with the launcher's own
/// version): `DS_PRODUCT_VERSION` when set at build time (the launcher's build sets it to its
/// version), the crate version otherwise.
pub const VERSION: &str = match option_env!("DS_PRODUCT_VERSION") {
    Some(v) if !v.is_empty() => v,
    _ => env!("CARGO_PKG_VERSION"),
};

/// The build's commit: 40 lowercase hex (set `DS_COMMIT` when building), else 40 zeros.
pub fn commit() -> &'static str {
    match option_env!("DS_COMMIT") {
        Some(c)
            if c.len() == 40
                && c.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) =>
        {
            c
        }
        _ => "0000000000000000000000000000000000000000",
    }
}
