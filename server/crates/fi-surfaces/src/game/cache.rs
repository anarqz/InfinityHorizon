//! The game plane's per-instance liveness cache (ADR 0008 §4 and its 2026-10-18 amendment,
//! docs/contracts/GAME_PLATFORM.md §1; review finding R4 of docs/server/LOCAL_ENV.md §12).
//!
//! The game plane checks a session's liveness "at most every 60 s": a positive answer read from
//! Postgres (session `active`, this pool) is kept for `FI_GAME_LIVENESS_CACHE_MS` (default 60000,
//! at most 60000, 0 disables), so a player's stream of game requests costs one `play_session` read
//! per TTL instead of one per request. It is **derived state, never shared state**: each instance
//! keeps its own copy, losing it changes nothing but one extra read, and no decision that another
//! instance must see is taken from it (admission, leases and saves read Postgres as before).
//!
//! What it may serve stale, by design: a session ended, expired or taken over keeps the game plane
//! of the instances that cached it for at most the TTL (the contract's "within a minute"). What it
//! never serves: a session past `game_until`, because the cached database clock is advanced by the
//! time since the read and [`fi_db::game::Liveness::serves`] is evaluated on every request; a
//! negative answer, which is never cached. Bounded: at most [`MAX_ENTRIES`] sessions, expired
//! entries pruned first, everything dropped when still full.
//!
//! Platform v1 adds two more of the same kind (derived, per instance, bounded, lost at no cost):
//! [`SeenBatch`], the sessions this instance served since its last liveness write (ADR 0015 §3:
//! `last_seen_at` at most once per session per minute, many sessions per statement), and
//! [`TtlCache`], the LRU + TTL cache of the universe's and matchmaking's read answers (ADR 0016
//! §2: positive answers only, keyed by the full query, invalidated by time only, hit and miss
//! counters).

use fi_db::game::{Liveness, Seen};
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The contract's bound: the game plane re-reads a session at least every 60 s.
pub const MAX_TTL_MS: u32 = 60_000;
/// Sessions one instance keeps (a game instance serves tens to hundreds).
pub const MAX_ENTRIES: usize = 50_000;

/// Positive liveness answers of this instance, by session id.
#[derive(Debug)]
pub struct LivenessCache {
    ttl: Duration,
    entries: Mutex<HashMap<Uuid, (Liveness, Instant)>>,
}

impl LivenessCache {
    /// A cache keeping answers `ttl_ms` (0: every request reads Postgres).
    pub fn new(ttl_ms: u32) -> Self {
        Self {
            ttl: Duration::from_millis(u64::from(ttl_ms.min(MAX_TTL_MS))),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The TTL in milliseconds (0 = disabled).
    pub fn ttl_ms(&self) -> u128 {
        self.ttl.as_millis()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, (Liveness, Instant)>> {
        // A panic while holding the lock cannot leave a half-written entry: recover the map.
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh cached answer, its database clock advanced by the time since it was read.
    pub fn get(&self, session: Uuid) -> Option<Liveness> {
        if self.ttl.is_zero() {
            return None;
        }
        let mut map = self.map();
        let (live, at) = map.get(&session)?;
        let age = at.elapsed();
        if age >= self.ttl {
            map.remove(&session);
            return None;
        }
        let mut live = live.clone();
        live.now = live
            .now
            .plus_millis(i64::try_from(age.as_millis()).unwrap_or(i64::MAX));
        Some(live)
    }

    /// Keeps `live` if it is a positive answer for `pool` (anything else is never cached).
    pub fn put(&self, live: &Liveness, pool: &str) {
        if self.ttl.is_zero() || !live.serves(pool) {
            return;
        }
        let mut map = self.map();
        if map.len() >= MAX_ENTRIES && !map.contains_key(&live.session_id) {
            let ttl = self.ttl;
            map.retain(|_, (_, at)| at.elapsed() < ttl);
            if map.len() >= MAX_ENTRIES {
                map.clear();
            }
        }
        map.insert(live.session_id, (live.clone(), Instant::now()));
    }

    /// Drops a session (its cached answer no longer admits a request).
    pub fn forget(&self, session: Uuid) {
        if !self.ttl.is_zero() {
            self.map().remove(&session);
        }
    }

    /// Sessions held now (tests).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map().len()
    }
}

/// Sessions seen by this instance whose `last_seen_at` is due (ADR 0015 §3).
#[derive(Debug)]
pub struct SeenBatch {
    every: Duration,
    state: Mutex<SeenState>,
}

#[derive(Debug, Default)]
struct SeenState {
    written: HashMap<Uuid, Instant>,
    pending: HashMap<Uuid, Seen>,
}

impl SeenBatch {
    /// Writes each session at most once per `every` (the contract's 60 s).
    pub fn new(every: Duration) -> Self {
        Self {
            every,
            state: Mutex::new(SeenState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SeenState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Notes that `seen` was served now; queued for the next flush when its last write is older
    /// than `every`.
    pub fn mark(&self, seen: Seen) {
        let mut s = self.lock();
        let due = s
            .written
            .get(&seen.session_id)
            .is_none_or(|at| at.elapsed() >= self.every);
        if due && !s.pending.contains_key(&seen.session_id) {
            if s.pending.len() >= MAX_ENTRIES {
                return;
            }
            s.pending.insert(seen.session_id, seen);
        }
    }

    /// Takes the due sessions (marked written now).
    pub fn take(&self) -> Vec<Seen> {
        let mut s = self.lock();
        let now = Instant::now();
        let every = self.every;
        if s.written.len() >= MAX_ENTRIES {
            s.written.retain(|_, at| at.elapsed() < every);
            if s.written.len() >= MAX_ENTRIES {
                s.written.clear();
            }
        }
        let taken: Vec<Seen> = s.pending.drain().map(|(_, v)| v).collect();
        for t in &taken {
            s.written.insert(t.session_id, now);
        }
        taken
    }

    /// Writes the due sessions: first the liveness signal the sweeper reads
    /// (`play_session.last_seen_at`, `fi_db::gate::touch_seen`, in batches of
    /// `fi_db::gate::TOUCH_BATCH`), then the presence rows (`game_presence`, one statement). On a
    /// failure they are queued again (both writes are idempotent). Returns the play sessions
    /// touched.
    pub async fn flush(&self, db: &fi_db::PgPool, instance: &str) -> u64 {
        let due = self.take();
        if due.is_empty() {
            return 0;
        }
        let ids: Vec<Uuid> = due.iter().map(|d| d.session_id).collect();
        let mut touched = 0;
        let mut written: Result<(), String> = Ok(());
        for chunk in ids.chunks(fi_db::gate::TOUCH_BATCH) {
            match fi_db::gate::touch_seen(db, chunk).await {
                Ok(n) => touched += n,
                Err(e) => {
                    written = Err(e.to_string());
                    break;
                }
            }
        }
        if written.is_ok()
            && let Err(e) = fi_db::game::touch_seen(db, &due, instance).await
        {
            written = Err(e.to_string());
        }
        match written {
            Ok(()) => touched,
            Err(e) => {
                tracing::warn!(error = %e, sessions = due.len(), "liveness write failed; retried next flush");
                let mut s = self.lock();
                for d in due {
                    s.written.remove(&d.session_id);
                    s.pending.insert(d.session_id, d);
                }
                0
            }
        }
    }

    /// Sessions waiting for the next flush.
    pub fn pending(&self) -> usize {
        self.lock().pending.len()
    }
}

/// The entries of a [`TtlCache`]: values with their store time and last-use tick, and the ticks
/// in use order (the least recently used first).
#[derive(Debug)]
struct Lru<K, V> {
    tick: u64,
    map: HashMap<K, (V, Instant, u64)>,
    order: BTreeMap<u64, K>,
}

impl<K: Eq + Hash + Clone, V> Lru<K, V> {
    fn touch(&mut self, key: &K) {
        self.tick += 1;
        let tick = self.tick;
        if let Some((_, _, used)) = self.map.get_mut(key) {
            self.order.remove(used);
            *used = tick;
            self.order.insert(tick, key.clone());
        }
    }

    fn remove(&mut self, key: &K) {
        if let Some((_, _, used)) = self.map.remove(key) {
            self.order.remove(&used);
        }
    }
}

/// A bounded LRU cache with a TTL per cache, for positive read answers. Lookups, stores and
/// evictions are `O(log n)` under one short lock.
#[derive(Debug)]
pub struct TtlCache<K, V> {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<Lru<K, V>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlCache<K, V> {
    /// At most `capacity` answers, each for `ttl` (zero disables the cache).
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity: capacity.max(1),
            entries: Mutex::new(Lru {
                tick: 0,
                map: HashMap::new(),
                order: BTreeMap::new(),
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lru<K, V>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The TTL.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// A fresh answer (counted as a hit), else `None` (a miss).
    pub fn get(&self, key: &K) -> Option<V> {
        let fresh = if self.ttl.is_zero() {
            None
        } else {
            let mut lru = self.lock();
            match lru.map.get(key) {
                Some((v, at, _)) if at.elapsed() < self.ttl => {
                    let v = v.clone();
                    lru.touch(key);
                    Some(v)
                }
                Some(_) => {
                    lru.remove(key);
                    None
                }
                None => None,
            }
        };
        match &fresh {
            Some(_) => self.hits.fetch_add(1, Ordering::Relaxed),
            None => self.misses.fetch_add(1, Ordering::Relaxed),
        };
        fresh
    }

    /// Keeps an answer; when full, drops the least recently used one (expired answers are the
    /// least recently used ones by construction, or soon are).
    pub fn put(&self, key: K, value: V) {
        if self.ttl.is_zero() {
            return;
        }
        let mut lru = self.lock();
        lru.remove(&key);
        while lru.map.len() >= self.capacity {
            let Some((&oldest, _)) = lru.order.iter().next() else {
                break;
            };
            if let Some(k) = lru.order.remove(&oldest) {
                lru.map.remove(&k);
            }
        }
        lru.tick += 1;
        let tick = lru.tick;
        lru.order.insert(tick, key.clone());
        lru.map.insert(key, (value, Instant::now(), tick));
    }

    /// `(hits, misses)` so far.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    /// Answers held.
    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_domain::UnixMs;

    fn live(state: &str, game_until_in_ms: i64) -> Liveness {
        let now = UnixMs::from_millis(1_800_000_000_000);
        Liveness {
            session_id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            profile_id: Uuid::now_v7(),
            pool: "vip".into(),
            state: state.into(),
            game_until: Some(now.plus_millis(game_until_in_ms)),
            cloud: true,
            dedicated: false,
            modded: false,
            integrity: true,
            now,
        }
    }

    #[test]
    fn only_positive_answers_are_kept_and_for_the_ttl_only() {
        let c = LivenessCache::new(50);
        let ok = live("active", 600_000);
        c.put(&ok, "vip");
        assert_eq!(
            c.get(ok.session_id).map(|l| l.session_id),
            Some(ok.session_id)
        );
        let ended = live("ended", 600_000);
        c.put(&ended, "vip");
        assert!(
            c.get(ended.session_id).is_none(),
            "a negative answer is never cached"
        );
        let other = live("active", 600_000);
        c.put(&other, "free");
        assert!(
            c.get(other.session_id).is_none(),
            "another pool's session is never cached"
        );
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.get(ok.session_id).is_none(), "expired after the TTL");
        assert_eq!(c.len(), 0, "the expired entry is dropped on read");
    }

    #[test]
    fn the_cached_clock_moves_so_game_until_still_holds() {
        let c = LivenessCache::new(MAX_TTL_MS);
        let soon = live("active", 30);
        c.put(&soon, "vip");
        assert!(c.get(soon.session_id).unwrap().serves("vip"));
        std::thread::sleep(Duration::from_millis(40));
        let later = c.get(soon.session_id).unwrap();
        assert!(!later.serves("vip"), "past game_until even from the cache");
        c.forget(soon.session_id);
        assert!(c.get(soon.session_id).is_none());
    }

    #[test]
    fn the_seen_batch_writes_a_session_once_per_period() {
        let b = SeenBatch::new(Duration::from_millis(40));
        let seen = |sid| Seen {
            session_id: sid,
            profile_id: Uuid::now_v7(),
            pool: "vip".into(),
        };
        let (a, c) = (Uuid::now_v7(), Uuid::now_v7());
        b.mark(seen(a));
        b.mark(seen(a));
        b.mark(seen(c));
        assert_eq!(b.take().len(), 2, "two sessions, one entry each");
        b.mark(seen(a));
        assert!(b.take().is_empty(), "written less than a period ago");
        std::thread::sleep(Duration::from_millis(50));
        b.mark(seen(a));
        assert_eq!(b.pending(), 1);
    }

    #[test]
    fn the_ttl_cache_counts_expires_and_evicts_the_least_recently_used() {
        let c: TtlCache<&str, u32> = TtlCache::new(Duration::from_millis(40), 2);
        assert_eq!(c.get(&"a"), None);
        c.put("a", 1);
        c.put("b", 2);
        assert_eq!(c.get(&"a"), Some(1));
        c.put("c", 3);
        assert_eq!(c.get(&"b"), None, "b was the least recently used");
        assert_eq!(c.get(&"a"), Some(1));
        assert_eq!(c.counts(), (2, 2));
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(c.get(&"a"), None, "expired");
        let off: TtlCache<u8, u8> = TtlCache::new(Duration::ZERO, 8);
        off.put(1, 1);
        assert!(off.get(&1).is_none() && off.is_empty());
    }

    #[test]
    fn disabled_and_bounded() {
        let off = LivenessCache::new(0);
        let l = live("active", 600_000);
        off.put(&l, "vip");
        assert!(off.get(l.session_id).is_none());
        assert_eq!(
            LivenessCache::new(120_000).ttl_ms(),
            60_000,
            "capped at the contract's 60 s"
        );
    }
}
