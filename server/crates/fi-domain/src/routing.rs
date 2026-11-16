//! Tiers and pool routing (ADR 0008, ACCOUNTS.md §7).
//!
//! The account plane decides where a launched session plays: VIP accounts go to the VIP pool,
//! everyone else to the free pool. Pools are separate deployments with separate capacity and
//! admission, so free load can never take a VIP slot. The decision reads the account's cached
//! entitlement (monetization's `entitlement` table, product `vip`, written by services/billing from the state
//! machine, crates/fi-entitlements) and re-checks the end of access against `now`, because a
//! cached `active` row can be older than its own `until`.
//!
//! Status and tier wire strings are the monetization contract (fi-entitlements `model.rs`):
//! never rename one.

use crate::time::UnixMs;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// What an account may use. Wire strings: `"free"`, `"vip"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Capped pool with a queue, minimum feature set.
    Free,
    /// Paid tier: VIP pool, full save history and restore, full portal.
    Vip,
}

impl Tier {
    /// Stable wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::Free => "free",
            Tier::Vip => "vip",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A tier string other than `"free"` or `"vip"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown tier")]
pub struct UnknownTier;

impl FromStr for Tier {
    type Err = UnknownTier;

    fn from_str(s: &str) -> Result<Self, UnknownTier> {
        match s {
            "free" => Ok(Tier::Free),
            "vip" => Ok(Tier::Vip),
            _ => Err(UnknownTier),
        }
    }
}

/// Entitlement status as cached on the account (same strings as `fi_entitlements::Status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitlementStatus {
    /// Never had VIP.
    None,
    /// Inside a paid or comped period.
    Active,
    /// Period over, renewal still expected: access continues until `until`.
    Grace,
    /// A dispute is open.
    Suspended,
    /// Lapsed normally.
    Expired,
    /// Refunded.
    RevokedRefund,
    /// Dispute lost.
    RevokedChargeback,
    /// Revoked by an operator.
    RevokedAdmin,
}

impl EntitlementStatus {
    /// Every status, for exhaustive checks against the database CHECK constraint.
    pub const ALL: [EntitlementStatus; 8] = [
        EntitlementStatus::None,
        EntitlementStatus::Active,
        EntitlementStatus::Grace,
        EntitlementStatus::Suspended,
        EntitlementStatus::Expired,
        EntitlementStatus::RevokedRefund,
        EntitlementStatus::RevokedChargeback,
        EntitlementStatus::RevokedAdmin,
    ];

    /// Stable wire string.
    pub const fn as_str(self) -> &'static str {
        match self {
            EntitlementStatus::None => "none",
            EntitlementStatus::Active => "active",
            EntitlementStatus::Grace => "grace",
            EntitlementStatus::Suspended => "suspended",
            EntitlementStatus::Expired => "expired",
            EntitlementStatus::RevokedRefund => "revoked_refund",
            EntitlementStatus::RevokedChargeback => "revoked_chargeback",
            EntitlementStatus::RevokedAdmin => "revoked_admin",
        }
    }

    /// Whether the status can grant VIP at all (it still needs `until` in the future).
    pub const fn grants_vip(self) -> bool {
        matches!(self, EntitlementStatus::Active | EntitlementStatus::Grace)
    }
}

impl FromStr for EntitlementStatus {
    type Err = UnknownStatus;

    fn from_str(s: &str) -> Result<Self, UnknownStatus> {
        EntitlementStatus::ALL
            .into_iter()
            .find(|v| v.as_str() == s)
            .ok_or(UnknownStatus)
    }
}

/// A status string this build does not know.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown entitlement status")]
pub struct UnknownStatus;

/// An account's cached access, as the account plane reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Access {
    /// Status as last materialized by billing.
    pub status: EntitlementStatus,
    /// When access ends if nothing else happens; `None` when there is no access to end.
    pub until: Option<UnixMs>,
}

impl Access {
    /// No entitlement row: a free account.
    pub const FREE: Access = Access {
        status: EntitlementStatus::None,
        until: None,
    };

    /// The tier at `now`. A `vip`-granting status without an end, or with an end in the past, is
    /// treated as free: access must always be bounded so a stale cache can never grant it forever.
    pub fn tier_at(&self, now: UnixMs) -> Tier {
        match (self.status.grants_vip(), self.until) {
            (true, Some(until)) if now < until => Tier::Vip,
            _ => Tier::Free,
        }
    }

    /// The latest instant a session ticket minted at `now` may be valid until: never past the end
    /// of VIP access (monetization ADR 0301 §8). `None` for free access (no VIP bound applies).
    pub fn vip_bound(&self, now: UnixMs) -> Option<UnixMs> {
        (self.tier_at(now) == Tier::Vip)
            .then_some(self.until)
            .flatten()
    }
}

/// A game-plane pool: a set of game-api deployments with its own capacity and admission queue.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PoolName(String);

/// A pool name outside `^[a-z][a-z0-9-]{1,30}$` (the `pool.name` CHECK constraint).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a pool name is 2 to 31 lower-case letters, digits or dashes, starting with a letter")]
pub struct InvalidPoolName;

impl PoolName {
    /// The VIP pool.
    pub fn vip() -> Self {
        Self("vip".to_owned())
    }

    /// The free pool on the new stack.
    pub fn free() -> Self {
        Self("free".to_owned())
    }

    /// The legacy Beta VM, while it still serves free profiles (ADR 0008 §4).
    pub fn legacy_free() -> Self {
        Self("legacy-free".to_owned())
    }

    /// Validates a pool name read from configuration or the database.
    pub fn new(name: &str) -> Result<Self, InvalidPoolName> {
        let ok = (2..=31).contains(&name.len())
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        ok.then(|| Self(name.to_owned())).ok_or(InvalidPoolName)
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PoolName {
    type Error = InvalidPoolName;

    fn try_from(value: String) -> Result<Self, InvalidPoolName> {
        Self::new(&value)
    }
}

impl From<PoolName> for String {
    fn from(value: PoolName) -> String {
        value.0
    }
}

impl fmt::Display for PoolName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which pool serves each tier. Configuration, not code: the free tier points at the legacy VM
/// until the free pool on the new stack is cut over (OPEN_QUESTIONS.md Q1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolRouting {
    /// Pool for VIP accounts.
    pub vip: PoolName,
    /// Pool for free accounts.
    pub free: PoolName,
}

impl PoolRouting {
    /// MVP routing: VIP on the new stack, free on the legacy VM.
    pub fn mvp() -> Self {
        Self {
            vip: PoolName::vip(),
            free: PoolName::legacy_free(),
        }
    }

    /// The pool for an account's access at `now`.
    pub fn pool_for(&self, access: &Access, now: UnixMs) -> &PoolName {
        match access.tier_at(now) {
            Tier::Vip => &self.vip,
            Tier::Free => &self.free,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: UnixMs = UnixMs::from_millis(1_800_000_000_000);

    #[test]
    fn vip_needs_a_granting_status_and_a_future_end() {
        let later = Some(NOW.plus_millis(60_000));
        let earlier = Some(NOW.plus_millis(-1));
        let active = |until| Access {
            status: EntitlementStatus::Active,
            until,
        };
        assert_eq!(active(later).tier_at(NOW), Tier::Vip);
        assert_eq!(
            active(earlier).tier_at(NOW),
            Tier::Free,
            "stale cache past its own end"
        );
        assert_eq!(
            active(None).tier_at(NOW),
            Tier::Free,
            "unbounded access is never granted"
        );
        assert_eq!(
            Access {
                status: EntitlementStatus::Grace,
                until: later
            }
            .tier_at(NOW),
            Tier::Vip
        );
        for status in [
            EntitlementStatus::None,
            EntitlementStatus::Suspended,
            EntitlementStatus::Expired,
            EntitlementStatus::RevokedRefund,
            EntitlementStatus::RevokedChargeback,
            EntitlementStatus::RevokedAdmin,
        ] {
            assert_eq!(
                Access {
                    status,
                    until: later
                }
                .tier_at(NOW),
                Tier::Free,
                "{status:?}"
            );
        }
    }

    #[test]
    fn tickets_never_outlive_vip() {
        let until = NOW.plus_millis(5 * 60_000);
        let vip = Access {
            status: EntitlementStatus::Active,
            until: Some(until),
        };
        assert_eq!(vip.vip_bound(NOW), Some(until));
        assert_eq!(Access::FREE.vip_bound(NOW), None);
    }

    #[test]
    fn pools_are_separate_per_tier() {
        let routing = PoolRouting::mvp();
        let vip = Access {
            status: EntitlementStatus::Active,
            until: Some(NOW.plus_millis(1)),
        };
        assert_eq!(routing.pool_for(&vip, NOW), &PoolName::vip());
        assert_eq!(
            routing.pool_for(&Access::FREE, NOW),
            &PoolName::legacy_free()
        );
    }

    #[test]
    fn status_strings_match_monetization() {
        let wire: Vec<&str> = EntitlementStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            wire,
            [
                "none",
                "active",
                "grace",
                "suspended",
                "expired",
                "revoked_refund",
                "revoked_chargeback",
                "revoked_admin"
            ]
        );
        for s in EntitlementStatus::ALL {
            assert_eq!(s.as_str().parse::<EntitlementStatus>(), Ok(s));
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
        }
    }

    #[test]
    fn tier_strings_round_trip() {
        for tier in [Tier::Free, Tier::Vip] {
            assert_eq!(tier.as_str().parse::<Tier>(), Ok(tier));
        }
        assert_eq!("Vip".parse::<Tier>(), Err(UnknownTier));
        assert_eq!("".parse::<Tier>(), Err(UnknownTier));
    }

    #[test]
    fn pool_names_are_validated() {
        assert!(PoolName::new("vip-us-east1").is_ok());
        assert!(PoolName::new("v").is_err());
        assert!(PoolName::new("VIP").is_err());
        assert!(PoolName::new("1vip").is_err());
    }
}
