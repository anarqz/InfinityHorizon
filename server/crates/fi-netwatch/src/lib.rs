//! The relay as the anti-cheat detector (MATCHMAKING_API.md §5d, OD-159/OD-160, register CR-S80).
//!
//! A leaf without I/O. The relay process (`fi-relay-vm`) owns one [`detect::Detector`] in its
//! rendezvous task, feeds it every `data` payload fi-relay forwards for a relayed grant with the
//! leg it crossed, ticks it once a second and ships its findings; the platform verifies what the
//! relay signs with the same [`cert`] module. The relay **detects**; the platform alone decides.
//!
//! | Module | What |
//! | --- | --- |
//! | [`nerd`] | the NerdCore reader: header, every message layout, CarPredictionFull, PlayerData's XUID |
//! | [`detect`] | per-second rings, owner maps, voids, violation/forgery/jump evaluation (preallocated) |
//! | [`cert`] | per-start certificates and body signatures, both directions |
//! | [`client`] | the relay's session, signed request bodies, checks of the platform's answers |

pub mod cert;
pub mod client;
pub mod detect;
pub mod nerd;

pub use detect::{Config, Counters, Detector, Finding, GrantInfo, Leg, Rules};
