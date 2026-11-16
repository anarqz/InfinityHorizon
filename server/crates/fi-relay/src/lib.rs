//! The relay's UDP core (ADR 0017 §2, MATCHMAKING_API.md §5), shared by the relay VM
//! (`services/relay`) and, later, the player-hosted relay of `infinity.exe relay`.
//!
//! The relay holds no database and no secret: it verifies relay grants with public keys
//! ([`grant`]), pairs the two sides of a grant so each learns the other's public endpoint, and
//! forwards for a capped number of peers that could not punch ([`rendezvous`]). The datagram
//! shapes are in [`wire`]; [`udp`] runs them on sockets.
//!
//! | Module | What |
//! | --- | --- |
//! | [`grant`] | `FI_RELAY_GRANT_VERIFY_KEYS` and the Ed25519 check of a grant |
//! | [`wire`] | binding answers (STUN shape) and the rendezvous datagrams |
//! | [`rendezvous`] | pairing, fallback cap, lifetimes, bounds (no I/O) |
//! | [`udp`] | the binding and rendezvous socket loops |

pub mod grant;
pub mod rendezvous;
pub mod udp;
pub mod wire;

pub use grant::{GrantError, VerifyKeys};
pub use rendezvous::{Config, Rendezvous};
