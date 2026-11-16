//! The in-memory store's clock. The Postgres store never uses one: it reads the database clock
//! inside the transaction that decides (CLOUD_SAVES.md §5.4), so instances with skewed clocks
//! cannot disagree about whether a lease has lapsed.

use fi_domain::UnixMs;
use std::sync::atomic::{AtomicI64, Ordering};

/// A source of "now".
pub trait Clock: Send + Sync {
    /// The current instant.
    fn now(&self) -> UnixMs;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixMs {
        UnixMs::now_system()
    }
}

/// A clock tests move by hand.
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    /// Starts at `start`.
    pub fn new(start: UnixMs) -> Self {
        Self(AtomicI64::new(start.millis()))
    }

    /// Moves forward (or back, for negative `ms`).
    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> UnixMs {
        UnixMs::from_millis(self.0.load(Ordering::SeqCst))
    }
}
