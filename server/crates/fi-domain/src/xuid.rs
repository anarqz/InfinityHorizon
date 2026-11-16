//! XUIDs: the 64-bit user id the game sees. The hook injects it into Azimuth's `LocalUser`, and
//! Azimuth names the profile's save folder after it (`User_<XUID in upper-case hex>`).
//!
//! Beta allocated `MAX(xuid)+1` above [`BETA_FLOOR`] and handed a deleted account's XUID to the
//! next registration (Beta brief §8, failure 14): stale per-XUID files on a PC could then act on
//! the wrong profile. The new allocator is a Postgres sequence starting at [`ALLOCATION_FLOOR`];
//! a XUID is never reused because profile rows are never hard-deleted (ACCOUNTS.md §4).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Every Beta XUID is above this value (`server/src/db.rs:807-812`).
pub const BETA_FLOOR: u64 = 2_535_000_000_000_000;
/// First XUID the new allocator hands out (`profile_xuid_seq` in crates/fi-db/migrations). Beta
/// XUIDs imported by the migration stay below it, so both allocators can run during dual-run
/// without ever colliding (docs/server/MIGRATION.md §3).
pub const ALLOCATION_FLOOR: u64 = 2_535_100_000_000_001;
/// Azimuth's own offline user (`User_9000000000000`): never an online profile.
pub const AZIMUTH_OFFLINE: u64 = 9_000_000_000_000;

/// A profile's XUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct Xuid(u64);

/// A value that cannot be an online profile's XUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum XuidError {
    /// Below the Beta range: Azimuth's offline user, or garbage.
    #[error("XUID {0} is below the online range")]
    OutOfRange(u64),
    /// Does not fit the signed 64-bit column it is stored in.
    #[error("XUID {0} does not fit a signed 64-bit integer")]
    TooLarge(u64),
}

impl Xuid {
    /// Validates a XUID read from a request, the database or a Beta export.
    pub fn new(value: u64) -> Result<Self, XuidError> {
        if value <= BETA_FLOOR {
            return Err(XuidError::OutOfRange(value));
        }
        if value > i64::MAX as u64 {
            return Err(XuidError::TooLarge(value));
        }
        Ok(Self(value))
    }

    /// The number.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Whether Beta allocated it (imported profile) or the new allocator did.
    pub const fn is_beta(self) -> bool {
        self.0 < ALLOCATION_FLOOR
    }

    /// Azimuth's save folder name for this profile: `User_` + upper-case hex, no leading zeros
    /// (`hook/adapters/azimuth_cloud.c`, `azimuth_cloud_start`).
    pub fn save_folder(self) -> String {
        format!("User_{:X}", self.0)
    }
}

impl TryFrom<u64> for Xuid {
    type Error = XuidError;

    fn try_from(value: u64) -> Result<Self, XuidError> {
        Self::new(value)
    }
}

impl From<Xuid> for u64 {
    fn from(value: Xuid) -> u64 {
        value.0
    }
}

impl fmt::Display for Xuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_user_and_garbage_are_refused() {
        assert_eq!(
            Xuid::new(AZIMUTH_OFFLINE),
            Err(XuidError::OutOfRange(AZIMUTH_OFFLINE))
        );
        assert_eq!(
            Xuid::new(BETA_FLOOR),
            Err(XuidError::OutOfRange(BETA_FLOOR))
        );
        assert_eq!(Xuid::new(u64::MAX), Err(XuidError::TooLarge(u64::MAX)));
    }

    #[test]
    fn beta_and_new_ranges_do_not_overlap() {
        let beta = Xuid::new(BETA_FLOOR + 4_321).unwrap();
        let new = Xuid::new(ALLOCATION_FLOOR).unwrap();
        assert!(beta.is_beta());
        assert!(!new.is_beta());
    }

    #[test]
    fn save_folder_matches_the_hook() {
        // 2535000000000001 = 0x901A_F5E8_8A801 (no leading zeros, upper case).
        let x = Xuid::new(2_535_000_000_000_001).unwrap();
        assert_eq!(
            x.save_folder(),
            format!("User_{:X}", 2_535_000_000_000_001u64)
        );
        assert!(
            x.save_folder()
                .chars()
                .skip(5)
                .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
        );
    }
}
