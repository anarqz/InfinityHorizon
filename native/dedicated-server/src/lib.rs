// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Derived in part from fh2-mods (https://github.com/conner-mcvay/fh2-mods), MIT, Copyright (c) 2026 conner-mcvay and
// contributors; see licenses/fh2-mods-LICENSE.txt.
//! `dedicated-server`: the Infinity world engine, one program for private worlds (a child process
//! of the launcher, one world each) and public worlds (a long-lived process on the relay VM).
//!
//! Layers:
//!   - `core`: the deterministic, I/O-free server: control commands, worlds, admission state,
//!     timers, events. Time and randomness come from the caller, datagrams leave through an
//!     output buffer. Golden vectors replay against this layer.
//!   - `core::game`: the trait between the core and a game module (OD-186: one game-agnostic
//!     core, one module per game; `world.open` names the game).
//!   - `fh2`: the Forza Horizon 2 module: NerdCore codec (`bits`, `nerd`), side channels
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

/// The build's version (Cargo) and commit (40 lowercase hex; set `DS_COMMIT` when building,
/// else 40 zeros).
/// The game modules this build hosts.
pub fn registered_games(fh2_catalog: fh2::Catalog) -> Vec<std::sync::Arc<dyn core::game::Game>> {
    vec![std::sync::Arc::new(fh2::Fh2::new(fh2_catalog))]
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
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
