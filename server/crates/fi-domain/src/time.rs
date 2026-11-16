//! Instants as Unix milliseconds.
//!
//! Server decisions (lease expiry, gates, ticket bounds) always use the **database clock** read
//! inside the deciding transaction, never an instance's or a player's clock (Beta brief §8,
//! failure 17: wall-clock steps shrank the gates' allowances). The pure crates take that instant
//! as a parameter; this type carries it. Milliseconds are exact in Postgres `timestamptz`
//! (microsecond precision), so values round-trip through the database unchanged.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Milliseconds since 1970-01-01T00:00:00Z.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMs(i64);

impl UnixMs {
    /// From a millisecond count.
    pub const fn from_millis(ms: i64) -> Self {
        Self(ms)
    }

    /// The millisecond count.
    pub const fn millis(self) -> i64 {
        self.0
    }

    /// This instant moved by `ms` (negative moves back); saturates instead of overflowing.
    pub const fn plus_millis(self, ms: i64) -> Self {
        Self(self.0.saturating_add(ms))
    }

    /// Milliseconds from `earlier` to `self` (negative when `earlier` is later).
    pub const fn since(self, earlier: UnixMs) -> i64 {
        self.0.saturating_sub(earlier.0)
    }

    /// The system clock. Only for clients and tests: servers use the database clock.
    pub fn now_system() -> Self {
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self(elapsed)
    }
}

impl fmt::Display for UnixMs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_saturates() {
        let t = UnixMs::from_millis(i64::MAX - 1);
        assert_eq!(t.plus_millis(10), UnixMs::from_millis(i64::MAX));
        assert_eq!(UnixMs::from_millis(10).since(UnixMs::from_millis(4)), 6);
        assert_eq!(UnixMs::from_millis(4).since(UnixMs::from_millis(10)), -6);
    }
}
