//! Bounded non-authoritative network telemetry; never used for admission or routing.
//! Event fields override untrusted request span text; validated trace context still correlates.
use std::sync::atomic::{AtomicU64, Ordering};

/// Emit only scalar allowlisted fields; do not inherit arbitrary URL/header/method strings.
#[macro_export]
macro_rules! network_info {
    ($($fields:tt)*) => { tracing::info!(path = "network_diagnostic", method = "DIAGNOSTIC", request_id = "withheld", $($fields)*); };
}
/// Warning equivalent of [`crate::network_info`].
#[macro_export]
macro_rules! network_warn {
    ($($fields:tt)*) => { tracing::warn!(path = "network_diagnostic", method = "DIAGNOSTIC", request_id = "withheld", $($fields)*); };
}

/// Fixed telemetry-only buckets, independent of any user-controlled value.
#[derive(Clone, Copy)]
pub enum Poll {
    Punches,
    Seat,
    Ticket,
    Session,
    RateLimit,
    Refusal,
    RouteHost,
    RouteGuest,
}
struct Budget {
    next: AtomicU64,
    suppressed: AtomicU64,
}
impl Budget {
    const fn new() -> Self {
        Self {
            next: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
        }
    }
    fn take(&self, now: u64, seconds: u64) -> Option<u64> {
        let next = self.next.load(Ordering::Relaxed);
        if now >= next
            && self
                .next
                .compare_exchange(
                    next,
                    now.saturating_add(seconds),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            Some(self.suppressed.swap(0, Ordering::Relaxed))
        } else {
            let _ = self
                .suppressed
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                    Some(v.saturating_add(1))
                });
            None
        }
    }
}
static BUDGETS: [Budget; 8] = [const { Budget::new() }; 8];
/// Coalesce repetitive polls per process. Restart may emit a fresh first sample; no authority lives here.
pub fn poll(kind: Poll) -> Option<u64> {
    let (index, seconds) = match kind {
        Poll::Punches => (0, 15),
        Poll::Seat => (1, 5),
        Poll::Ticket => (2, 5),
        Poll::Session => (3, 5),
        Poll::RateLimit => (4, 5),
        Poll::Refusal => (5, 1),
        Poll::RouteHost => (6, 1),
        Poll::RouteGuest => (7, 1),
    };
    BUDGETS[index].take(crate::state::now_secs().max(0) as u64, seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn poll_flood_is_bounded_and_drops_are_counted_without_affecting_callers() {
        let budget = Budget::new();
        assert_eq!(budget.take(100, 5), Some(0));
        for _ in 0..10_000 {
            assert_eq!(budget.take(101, 5), None);
        }
        assert_eq!(budget.take(105, 5), Some(10_000));
        assert_eq!(budget.take(106, 5), None);
    }
}
