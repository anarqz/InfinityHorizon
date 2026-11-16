//! The save store boundary (CLOUD_SAVES.md §5).
//!
//! Every write method runs its decision ([`crate::lease`]) and its write in **one critical
//! section** per line: the row lock of `save_head` in Postgres (crates/fi-db `PgSaveStore`), the
//! mutex in memory ([`crate::memory::MemorySaveStore`]). The conformance suite
//! (`crate::conformance`, feature `testkit`) runs the same scenarios against both.
//!
//! Blob bytes never pass through the store: the caller puts the bundle in the blob store first
//! (content-addressed, so a retry is harmless), then commits its digest here. A crash between the
//! two leaves only an unreferenced blob, which garbage collection adopts and later deletes
//! (CLOUD_SAVES.md §6). A commit or park that creates the blob's row says so
//! (`blob_created`), and the caller then puts the object again.
//!
//! The Postgres store also exposes every write as a function over a caller's transaction
//! (`fi_db::saves::{acquire_in, renew_in, release_in, commit_in}`), so the account plane can lock
//! a pool, grant the lease and open the play session atomically.

use crate::error::SaveError;
use crate::model::{
    AcquireRequest, CommitId, CommitRequest, Committed, FenceToken, HeadView, LeaseGrant,
    ParkRequest, ParkedRecord, Released, Revision, RevisionRecord,
};
use fi_domain::{PlaySessionId, ProfileId};
use std::future::Future;

/// Durable storage of save lines, leases and parked bundles.
pub trait SaveStore: Send + Sync {
    /// Creates the profile's empty line (revision 0) if it does not exist. Idempotent. The line's
    /// mode is the profile's (Postgres: `profile.mode`; in memory: Noob unless created with
    /// `MemorySaveStore::ensure_line_as`).
    fn ensure_line(&self, profile: ProfileId)
    -> impl Future<Output = Result<(), SaveError>> + Send;

    /// The head, the stored lease and the store's clock. Takes no lock.
    fn head(&self, profile: ProfileId) -> impl Future<Output = Result<HeadView, SaveError>> + Send;

    /// Acquires the line's lease ([`crate::lease::decide_acquire`]).
    fn acquire(
        &self,
        profile: ProfileId,
        req: AcquireRequest,
    ) -> impl Future<Output = Result<LeaseGrant, SaveError>> + Send;

    /// Renews a live lease ([`crate::lease::decide_renew`]).
    fn renew(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
        ttl_ms: u32,
    ) -> impl Future<Output = Result<crate::model::Lease, SaveError>> + Send;

    /// Releases the session's lease ([`crate::lease::decide_release`]).
    fn release(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
    ) -> impl Future<Output = Result<Released, SaveError>> + Send;

    /// Commits one revision ([`crate::lease::check_commit`]), idempotent by `commit_id`. The
    /// blob row is registered in the same critical section.
    fn commit(
        &self,
        req: CommitRequest,
    ) -> impl Future<Output = Result<Committed, SaveError>> + Send;

    /// Newest-first history, up to `limit` revisions older than `before` (all when `None`).
    fn history(
        &self,
        profile: ProfileId,
        before: Option<Revision>,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<RevisionRecord>, SaveError>> + Send;

    /// The first revision of the guardrails' rate window (CLOUD_SAVES.md §7.3). Among the
    /// revisions at or below `at_or_below` with readable numbers
    /// ([`crate::SaveSummary::is_complete_for_gates`]) and not older than the newest rate anchor
    /// at or below it ([`crate::CommitKind::is_rate_anchor`]: a fresh career, a Beta import, a
    /// reset, an accepted parked bundle; not a restore): the newest one whose play time is below
    /// `min_play_ms`, so the step into the window is counted (review G2-5); else the oldest one
    /// (review G2-12). `None` when there is none (or no line).
    fn play_window_start(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
        min_play_ms: u64,
    ) -> impl Future<Output = Result<Option<RevisionRecord>, SaveError>> + Send;

    /// The spin allowance's level high-water mark at `at_or_below` (review G2-3): the highest
    /// readable level of the revisions at or below it and not older than the newest revision that
    /// restarts the mark ([`crate::CommitKind::restarts_level_high`]: a rate anchor or a restore).
    /// `None` when there is none (or no line).
    fn level_high_water(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
    ) -> impl Future<Output = Result<Option<i64>, SaveError>> + Send;

    /// One revision.
    fn revision(
        &self,
        profile: ProfileId,
        revision: Revision,
    ) -> impl Future<Output = Result<RevisionRecord, SaveError>> + Send;

    /// The revision an idempotency key committed, on any line (keys are unique across lines), if
    /// any: how the service answers a retry whose head has already moved without committing.
    fn find_commit(
        &self,
        commit_id: CommitId,
    ) -> impl Future<Output = Result<Option<RevisionRecord>, SaveError>> + Send;

    /// Keeps a bundle outside the line (conflict loser, refused, unreadable, pre-restore copy).
    fn park(
        &self,
        req: ParkRequest,
    ) -> impl Future<Output = Result<ParkedRecord, SaveError>> + Send;

    /// The profile's parked bundles that retention has not dropped, oldest first.
    fn parked(
        &self,
        profile: ProfileId,
    ) -> impl Future<Output = Result<Vec<ParkedRecord>, SaveError>> + Send;
}
