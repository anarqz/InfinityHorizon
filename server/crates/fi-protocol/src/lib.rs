//! Wire contracts of the v1 client APIs (docs/contracts/ACCOUNTS.md, SAVES_API.md, PORTAL_API.md,
//! GAME_PLATFORM.md, and the platform v1 surfaces CORE_API.md, GATE_API.md, UNIVERSE_API.md,
//! MATCHMAKING_API.md, whose DTO modules the lanes fill), and the beholder of ANNOUNCER_API.md.
//!
//! Request and response bodies, route paths, header names, and the error envelope with every
//! stable code and its HTTP status. The TypeScript mirror is `packages/contracts`
//! (`@forza-infinity/contracts`); tests/parity.rs keeps the two error tables equal and checks that
//! every `fi_saves::SaveError` code is listed here.
//!
//! Conventions: camelCase fields; ids are UUID strings; XUIDs are decimal strings; digests are
//! lower-case hex; instants are RFC 3339 UTC with six fractional digits ([`instant`]). Request
//! bodies refuse unknown fields; clients must ignore unknown response fields.

pub mod accounts;
pub mod beholder;
pub mod content;
pub mod core;
pub mod crash;
pub mod dashboard;
pub mod error;
pub mod game;
pub mod gate;
pub mod headers;
pub mod instant;
pub mod matchmaking;
pub mod online_control;
pub mod portal;
pub mod reference;
pub mod relay_watch;
pub mod routes;
pub mod saves;
pub mod universe;
pub mod update;

pub use error::{ErrorBody, ErrorCode};
