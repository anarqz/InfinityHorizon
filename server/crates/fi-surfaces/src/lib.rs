//! fi-api as a library: the surface routers of the account plane and the game plane (ADR 0002),
//! mounted by this package's own binary (`main.rs`, the local environment v0's `api`,
//! `game-vip` and `game-free`) and, from the platform v1 split (ADR 0013), by the thin deployables
//! `services/core` (identity, portal, internal), `services/gate` (play sessions) and
//! `services/saves` (the save line, the game plane's platform surface). Nothing here is rewritten
//! for the split: a deployable composes [`launcher::router`], [`portal::router`],
//! [`game::router`] and [`internal::router`] with its own routes over the same [`state::AppState`]
//! and [`state::Platform`] extension that [`app::router`] installs.
//!
//! Routes by milestone (docs/milestones/server.md): health and meta (SER-M0); the identity routes
//! of `/launcher/v1` and `/portal/v1` (SER-1.1: Identity Platform sign-in, accounts, token
//! families, `/me`, deletion); play sessions and saves (SER-M2); the game plane (SER-M3); admission
//! across pools (SER-M4); `/internal/v1` push and scheduler endpoints (SER-M5). The local
//! environment v0 (docs/server/LOCAL_ENV.md) brings play sessions, saves, the game plane's
//! platform surface (`game`) and the internal pool endpoints (`internal`) forward on `main`.

pub mod app;
pub mod config;
pub mod content;
pub mod dashboard;
pub mod diagnostics;
#[cfg(test)]
mod e2e;
pub mod errors;
pub mod extract;
pub mod game;
pub mod gamedata;
pub mod internal;
pub mod jwks;
pub mod launcher;
pub mod launcher_policy;
pub mod mail;
pub mod media;
pub mod online_control;
pub mod portal;
pub mod signin;
pub mod state;
pub mod views;
pub mod worlds;
