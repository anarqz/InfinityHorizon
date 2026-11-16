//! In-process token buckets (ADR 0903 §3): the cheap layer below Cloudflare's edge rule and above
//! the exact Postgres limits of ACCOUNTS.md §9. A bucket is keyed by `(route class, subject)`
//! (the caller builds the key: `public_write:v4:203.0.113`, `account_write:<uuid>`), holds
//! `capacity` tokens and refills `capacity` tokens per `period`, continuously.
//!
//! Per instance by design: Cloud Run runs a few instances at most, so a flood against one of them
//! stops there without a shared store, and the limits that must be exact across instances stay in
//! Postgres. Derived state whose loss costs nothing but a fresh bucket (services/AGENTS.md). The
//! map is bounded: at `max_keys` a new key first drops the buckets that refilled completely (they
//! carry no information), in one scan at most every [`SWEEP_EVERY`], so a flood of distinct keys
//! never turns every request into a scan under the lock. While the map is still full a new key is
//! refused (`Limited`, 1 s): a bucket that is throttling someone is never dropped to make room,
//! and every bucket refills within its period, so the room comes back by itself.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default bound on the number of buckets of one [`TokenBuckets`].
pub const DEFAULT_MAX_KEYS: usize = 50_000;

/// Shortest time between two scans for full buckets when the map is at its bound.
pub const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// `capacity` requests per `period`, refilled continuously.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rate {
    /// Burst size and tokens per period (at least 1).
    pub capacity: u32,
    /// The refill period (non-zero).
    pub period: Duration,
}

impl Rate {
    /// `n` requests per minute.
    pub const fn per_minute(n: u32) -> Self {
        Self {
            capacity: n,
            period: Duration::from_secs(60),
        }
    }

    fn per_sec(self) -> f64 {
        f64::from(self.capacity.max(1)) / self.period.as_secs_f64().max(f64::EPSILON)
    }
}

/// The answer of [`TokenBuckets::take`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// A token was taken.
    Allowed,
    /// Empty: retry after this many whole seconds (`Retry-After`, at least 1).
    Limited {
        /// Seconds until one token is back.
        retry_after_secs: u64,
    },
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    updated: Instant,
    capacity: f64,
    per_sec: f64,
}

impl Bucket {
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
        self.updated = self.updated.max(now);
    }

    fn full_at(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens + elapsed * self.per_sec >= self.capacity
    }
}

#[derive(Debug, Default)]
struct Map {
    buckets: HashMap<String, Bucket>,
    /// The last scan for full buckets (none yet: the first full miss scans).
    swept: Option<Instant>,
}

/// A bounded set of token buckets (see the module documentation).
#[derive(Debug)]
pub struct TokenBuckets {
    buckets: Mutex<Map>,
    max_keys: usize,
}

impl Default for TokenBuckets {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_KEYS)
    }
}

impl TokenBuckets {
    /// At most `max_keys` buckets (at least 1).
    pub fn new(max_keys: usize) -> Self {
        Self {
            buckets: Mutex::new(Map::default()),
            max_keys: max_keys.max(1),
        }
    }

    /// Takes one token from the bucket `key` at `rate`, by the monotonic clock.
    pub fn take(&self, key: &str, rate: Rate) -> Decision {
        self.take_at(key, rate, Instant::now())
    }

    /// [`Self::take`] at `now` (tests drive the clock).
    pub fn take_at(&self, key: &str, rate: Rate, now: Instant) -> Decision {
        let mut guard = match self.buckets.lock() {
            Ok(guard) => guard,
            // A panic while holding the lock left a consistent map (no partial update is visible
            // across the lock), so the buckets stay usable.
            Err(poisoned) => poisoned.into_inner(),
        };
        let Map {
            buckets: map,
            swept,
        } = &mut *guard;
        if !map.contains_key(key) && map.len() >= self.max_keys {
            if swept.is_none_or(|at| now.saturating_duration_since(at) >= SWEEP_EVERY) {
                map.retain(|_, b| !b.full_at(now));
                *swept = Some(now);
            }
            if map.len() >= self.max_keys {
                return Decision::Limited {
                    retry_after_secs: 1,
                };
            }
        }
        let capacity = f64::from(rate.capacity.max(1));
        let per_sec = rate.per_sec();
        let bucket = map.entry(key.to_owned()).or_insert(Bucket {
            tokens: capacity,
            updated: now,
            capacity,
            per_sec,
        });
        // A rate change for a key (configuration) applies from now on.
        bucket.capacity = capacity;
        bucket.per_sec = per_sec;
        bucket.refill(now);
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Decision::Allowed
        } else {
            let wait = (1.0 - bucket.tokens) / per_sec;
            Decision::Limited {
                retry_after_secs: (wait.ceil() as u64).max(1),
            }
        }
    }

    /// Buckets held now.
    pub fn len(&self) -> usize {
        self.buckets.lock().map(|m| m.buckets.len()).unwrap_or(0)
    }

    /// No bucket held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eleventh_request_in_a_minute_waits_for_one_token() {
        let b = TokenBuckets::default();
        let t0 = Instant::now();
        let rate = Rate::per_minute(10);
        for i in 0..10 {
            assert_eq!(
                b.take_at("w:ip", rate, t0 + Duration::from_millis(i)),
                Decision::Allowed
            );
        }
        assert_eq!(
            b.take_at("w:ip", rate, t0 + Duration::from_millis(20)),
            Decision::Limited {
                retry_after_secs: 6
            },
            "one token every 6 s"
        );
        assert_eq!(
            b.take_at("w:other", rate, t0),
            Decision::Allowed,
            "keys are independent"
        );
        assert_eq!(
            b.take_at("w:ip", rate, t0 + Duration::from_secs(7)),
            Decision::Allowed
        );
        assert!(matches!(
            b.take_at("w:ip", rate, t0 + Duration::from_secs(7)),
            Decision::Limited { .. }
        ));
        // A full minute later the burst is back, never more than the capacity.
        for _ in 0..10 {
            assert_eq!(
                b.take_at("w:ip", rate, t0 + Duration::from_secs(600)),
                Decision::Allowed
            );
        }
        assert!(matches!(
            b.take_at("w:ip", rate, t0 + Duration::from_secs(600)),
            Decision::Limited { .. }
        ));
    }

    #[test]
    fn the_map_is_bounded_and_drops_full_buckets_first() {
        let b = TokenBuckets::new(3);
        let t0 = Instant::now();
        let rate = Rate::per_minute(2);
        b.take_at("a", rate, t0);
        b.take_at("a", rate, t0);
        b.take_at("b", rate, t0);
        b.take_at("c", rate, t0 + Duration::from_secs(1));
        assert_eq!(b.len(), 3);
        // After 60 s every bucket refilled: they are dropped to make room.
        b.take_at("d", rate, t0 + Duration::from_secs(120));
        assert_eq!(b.len(), 1);
        // Without full buckets nothing is dropped: a new key waits, the throttled "e" stays so.
        let t1 = t0 + Duration::from_secs(200);
        for (i, k) in ["d", "e", "f"].iter().enumerate() {
            b.take_at(k, rate, t1 + Duration::from_millis(i as u64));
            b.take_at(k, rate, t1 + Duration::from_millis(i as u64));
        }
        assert_eq!(b.len(), 3);
        assert!(matches!(b.take_at("e", rate, t1), Decision::Limited { .. }));
        assert_eq!(
            b.take_at("g", rate, t1 + Duration::from_secs(2)),
            Decision::Limited {
                retry_after_secs: 1
            }
        );
        assert_eq!(b.len(), 3);
        assert!(matches!(
            b.take_at("e", rate, t1 + Duration::from_secs(2)),
            Decision::Limited { .. }
        ));
        // Within SWEEP_EVERY of the last scan a miss does not scan again, even with room to win.
        let t2 = t0 + Duration::from_secs(400);
        assert_eq!(b.take_at("h", rate, t2), Decision::Allowed, "scan, room");
        b.take_at("h", rate, t2);
        b.take_at("i", rate, t2);
        b.take_at("i", rate, t2);
        let fast = Rate {
            capacity: 1,
            period: Duration::from_millis(1),
        };
        assert_eq!(b.take_at("k", fast, t2), Decision::Allowed);
        assert_eq!(b.len(), 3);
        // "k" is full again 1 ms later, but the last scan was at t2: no second scan yet.
        let soon = t2 + Duration::from_millis(5);
        assert!(matches!(
            b.take_at("j", rate, soon),
            Decision::Limited { .. }
        ));
        assert_eq!(
            b.take_at("j", rate, t2 + SWEEP_EVERY),
            Decision::Allowed,
            "the next scan drops the full bucket"
        );
        assert_eq!(b.len(), 3);
        assert!(!b.is_empty());
    }

    #[test]
    fn a_clock_that_goes_backwards_never_mints_tokens() {
        let b = TokenBuckets::default();
        let t0 = Instant::now() + Duration::from_secs(10);
        let rate = Rate::per_minute(1);
        assert_eq!(b.take_at("k", rate, t0), Decision::Allowed);
        assert!(matches!(
            b.take_at("k", rate, t0 - Duration::from_secs(5)),
            Decision::Limited { .. }
        ));
    }
}
