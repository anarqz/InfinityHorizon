//! Identity value types and routing rules for ForzaInfinity (ADR 0006, docs/contracts/ACCOUNTS.md).
//!
//! Everything here is pure: parsing, formatting and decisions over values the caller passes in,
//! so the account service, the game plane, the launcher and the tools apply the same rules. Wire
//! strings (`"hardcore"`, `"vip"`, `"Name#12345"`, `"K7QM-4XPA-9RT2"`) are part of the public
//! contract and never change once released.

pub mod activity;
pub mod game;
pub mod gamertag;
pub mod geo;
pub mod ids;
pub mod mode;
pub mod routing;
pub mod site_key;
pub mod time;
pub mod version;
pub mod xuid;

pub use game::{FH2, FM5, GameRules};
pub use gamertag::{Gamertag, GamertagError, GamertagInput};
pub use ids::{AccountId, DeviceId, NilId, PlaySessionId, ProfileId};
pub use mode::{ModeError, ProfileMode};
pub use routing::{Access, EntitlementStatus, PoolName, PoolRouting, Tier, UnknownTier};
pub use site_key::{SiteKey, SiteKeyError};
pub use time::UnixMs;
pub use xuid::{Xuid, XuidError};
