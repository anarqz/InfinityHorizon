//! In-memory save store: the reference implementation the property and loom tests check, and the
//! store of the launcher's preview mode. One mutex guards all lines, so every operation is
//! linearizable by construction; decisions are the shared ones of [`crate::lease`].
//!
//! [`MemoryCore`] is the synchronous core (explicit `now`), [`MemorySaveStore`] wraps it with a
//! [`Clock`] behind the async [`SaveStore`] trait. A line remembers its profile's mode (Noob unless
//! created with [`MemoryCore::ensure_line_as`]); Postgres reads `profile.mode` instead.

use crate::clock::Clock;
use crate::error::SaveError;
use crate::lease::{self, LineState};
use crate::model::{
    AcquireRequest, CommitId, CommitRequest, Committed, Digest, FenceToken, HeadView, Lease,
    LeaseGrant, ParkRequest, ParkedRecord, Released, Revision, RevisionRecord,
};
use crate::primitives::Mutex;
use crate::store::SaveStore;
use fi_domain::{PlaySessionId, ProfileId, ProfileMode, UnixMs};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug)]
struct Line {
    mode: ProfileMode,
    revision: Revision,
    fence_seq: u64,
    reset_revision: Revision,
    /// Play time credited so far (the live lease's share since `accounted_at` comes on top).
    play_ms: u64,
    lease: Option<Lease>,
    revisions: BTreeMap<Revision, RevisionRecord>,
}

impl Line {
    fn new(mode: ProfileMode) -> Self {
        Self {
            mode,
            revision: Revision::NONE,
            fence_seq: 0,
            reset_revision: Revision::NONE,
            play_ms: 0,
            lease: None,
            revisions: BTreeMap::new(),
        }
    }

    fn state(&self) -> LineState<'_> {
        LineState {
            mode: self.mode,
            revision: self.revision,
            fence_seq: self.fence_seq,
            reset_revision: self.reset_revision,
            lease: self.lease.as_ref(),
        }
    }

    /// Credits the stored lease's play time up to `now` (every write path calls this first).
    fn credit(&mut self, now: UnixMs) {
        self.play_ms = self
            .play_ms
            .saturating_add(lease::play_credit(self.lease.as_ref(), now));
    }
}

#[derive(Debug, Default)]
struct State {
    lines: HashMap<ProfileId, Line>,
    /// Idempotency keys are unique across all lines, as `save_revision.commit_id` is.
    commits: HashMap<CommitId, (ProfileId, Revision)>,
    blobs: HashMap<Digest, u32>,
    parked: Vec<ParkedRecord>,
}

impl State {
    /// Registers a blob row; `true` when it did not exist.
    fn insert_blob(&mut self, sha256: Digest, size: u32) -> bool {
        match self.blobs.entry(sha256) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(size);
                true
            }
        }
    }
}

/// The synchronous core.
#[derive(Debug, Default)]
pub struct MemoryCore {
    state: Mutex<State>,
}

fn poisoned<T>(_: T) -> SaveError {
    SaveError::Unavailable("memory store lock poisoned".into())
}

impl MemoryCore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// See [`SaveStore::ensure_line`]: a Noob line.
    pub fn ensure_line(&self, profile: ProfileId) -> Result<(), SaveError> {
        self.ensure_line_as(profile, ProfileMode::Noob)
    }

    /// Creates the profile's empty line with `mode` if it does not exist (an existing line keeps
    /// its mode).
    pub fn ensure_line_as(&self, profile: ProfileId, mode: ProfileMode) -> Result<(), SaveError> {
        self.state
            .lock()
            .map_err(poisoned)?
            .lines
            .entry(profile)
            .or_insert_with(|| Line::new(mode));
        Ok(())
    }

    /// The profile's mode changed (Hardcore to Noob, `POST .../mode`).
    pub fn set_mode(&self, profile: ProfileId, mode: ProfileMode) -> Result<(), SaveError> {
        let mut st = self.state.lock().map_err(poisoned)?;
        st.lines
            .get_mut(&profile)
            .ok_or(SaveError::ProfileNotFound)?
            .mode = mode;
        Ok(())
    }

    /// See [`SaveStore::head`].
    pub fn head(&self, profile: ProfileId, now: UnixMs) -> Result<HeadView, SaveError> {
        let st = self.state.lock().map_err(poisoned)?;
        let line = st.lines.get(&profile).ok_or(SaveError::ProfileNotFound)?;
        Ok(HeadView {
            profile,
            mode: line.mode,
            revision: line.revision,
            current: line.revisions.get(&line.revision).cloned(),
            lease: line.lease.clone(),
            fence_seq: line.fence_seq,
            reset_revision: line.reset_revision,
            play_ms: lease::play_ms_at(line.play_ms, line.lease.as_ref(), now),
            now,
        })
    }

    /// See [`SaveStore::acquire`].
    pub fn acquire(
        &self,
        profile: ProfileId,
        req: &AcquireRequest,
        now: UnixMs,
    ) -> Result<LeaseGrant, SaveError> {
        let mut st = self.state.lock().map_err(poisoned)?;
        let line = st
            .lines
            .get_mut(&profile)
            .ok_or(SaveError::ProfileNotFound)?;
        let grant = lease::decide_acquire(line.state(), req, now)?;
        if !grant.existing {
            line.credit(now);
            line.fence_seq = grant.lease.token.get();
            line.lease = Some(grant.lease.clone());
        }
        Ok(grant)
    }

    /// See [`SaveStore::renew`].
    pub fn renew(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
        ttl_ms: u32,
        now: UnixMs,
    ) -> Result<Lease, SaveError> {
        let mut st = self.state.lock().map_err(poisoned)?;
        let line = st
            .lines
            .get_mut(&profile)
            .ok_or(SaveError::ProfileNotFound)?;
        let renewed = lease::decide_renew(line.state(), session, token, ttl_ms, now)?;
        line.credit(now);
        line.lease = Some(renewed.clone());
        Ok(renewed)
    }

    /// See [`SaveStore::release`].
    pub fn release(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
        now: UnixMs,
    ) -> Result<Released, SaveError> {
        let mut st = self.state.lock().map_err(poisoned)?;
        let line = st
            .lines
            .get_mut(&profile)
            .ok_or(SaveError::ProfileNotFound)?;
        let outcome = lease::decide_release(line.state(), session, token)?;
        if outcome == Released::Released {
            line.credit(now);
            line.lease = None;
        }
        Ok(outcome)
    }

    /// See [`SaveStore::commit`].
    pub fn commit(&self, req: &CommitRequest, now: UnixMs) -> Result<Committed, SaveError> {
        let mut st = self.state.lock().map_err(poisoned)?;
        if let Some((owner, revision)) = st.commits.get(&req.commit_id) {
            let record = st.lines.get(owner).and_then(|l| l.revisions.get(revision));
            return match record {
                Some(record) if *owner == req.profile && record.blob.sha256 == req.blob.sha256 => {
                    Ok(Committed {
                        record: record.clone(),
                        replayed: true,
                        blob_created: false,
                    })
                }
                _ => Err(SaveError::IdempotencyKeyReused),
            };
        }
        let line = st
            .lines
            .get(&req.profile)
            .ok_or(SaveError::ProfileNotFound)?;
        let extended = lease::check_commit(line.state(), req, now)?;
        let blob_created = st.insert_blob(req.blob.sha256, req.blob.size);
        let line = st
            .lines
            .get_mut(&req.profile)
            .ok_or(SaveError::ProfileNotFound)?;
        line.credit(now);
        let revision = line.revision.next();
        let record = RevisionRecord {
            profile: req.profile,
            revision,
            commit_id: req.commit_id,
            blob: req.blob,
            content: req.content,
            kind: req.kind,
            actor: req.actor,
            session: req.session,
            token: req.token,
            device: req.device,
            client_label: req.client_label.clone(),
            restored_from: req.restored_from,
            parked_from: req.parked_from,
            summary: req.summary,
            committed_at: now,
            play_ms: line.play_ms,
            pinned: false,
        };
        line.revision = revision;
        line.reset_revision = lease::reset_after(req.kind, revision, line.reset_revision);
        line.lease = Some(extended);
        line.revisions.insert(revision, record.clone());
        st.commits.insert(req.commit_id, (req.profile, revision));
        Ok(Committed {
            record,
            replayed: false,
            blob_created,
        })
    }

    /// See [`SaveStore::history`].
    pub fn history(
        &self,
        profile: ProfileId,
        before: Option<Revision>,
        limit: u32,
    ) -> Result<Vec<RevisionRecord>, SaveError> {
        let st = self.state.lock().map_err(poisoned)?;
        let line = st.lines.get(&profile).ok_or(SaveError::ProfileNotFound)?;
        let upper = before.unwrap_or(Revision::new(u64::MAX));
        Ok(line
            .revisions
            .range(..upper)
            .rev()
            .take(limit as usize)
            .map(|(_, r)| r.clone())
            .collect())
    }

    /// See [`SaveStore::play_window_start`].
    pub fn play_window_start(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
        min_play_ms: u64,
    ) -> Result<Option<RevisionRecord>, SaveError> {
        let st = self.state.lock().map_err(poisoned)?;
        let Some(line) = st.lines.get(&profile) else {
            return Ok(None);
        };
        let upto = || line.revisions.range(..=at_or_below);
        let anchor = upto()
            .rev()
            .find(|(_, r)| r.kind.is_rate_anchor())
            .map_or(Revision::NONE, |(n, _)| *n);
        let readable =
            || upto().filter(move |(n, r)| **n >= anchor && r.summary.is_complete_for_gates());
        Ok(readable()
            .rev()
            .find(|(_, r)| r.play_ms < min_play_ms)
            .or_else(|| readable().next())
            .map(|(_, r)| r.clone()))
    }

    /// See [`SaveStore::level_high_water`].
    pub fn level_high_water(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
    ) -> Result<Option<i64>, SaveError> {
        let st = self.state.lock().map_err(poisoned)?;
        let Some(line) = st.lines.get(&profile) else {
            return Ok(None);
        };
        let upto = || line.revisions.range(..=at_or_below);
        let mark = upto()
            .rev()
            .find(|(_, r)| r.kind.restarts_level_high())
            .map_or(Revision::NONE, |(n, _)| *n);
        Ok(upto()
            .filter(|(n, _)| **n >= mark)
            .filter_map(|(_, r)| r.summary.level)
            .max())
    }

    /// See [`SaveStore::revision`].
    pub fn revision(
        &self,
        profile: ProfileId,
        revision: Revision,
    ) -> Result<RevisionRecord, SaveError> {
        let st = self.state.lock().map_err(poisoned)?;
        let line = st.lines.get(&profile).ok_or(SaveError::ProfileNotFound)?;
        line.revisions
            .get(&revision)
            .cloned()
            .ok_or(SaveError::RevisionNotFound(revision))
    }

    /// See [`SaveStore::find_commit`].
    pub fn find_commit(&self, commit_id: CommitId) -> Result<Option<RevisionRecord>, SaveError> {
        let st = self.state.lock().map_err(poisoned)?;
        Ok(st.commits.get(&commit_id).and_then(|(owner, revision)| {
            st.lines
                .get(owner)
                .and_then(|l| l.revisions.get(revision))
                .cloned()
        }))
    }

    /// See [`SaveStore::park`].
    pub fn park(&self, req: &ParkRequest, now: UnixMs) -> Result<ParkedRecord, SaveError> {
        let mut st = self.state.lock().map_err(poisoned)?;
        if !st.lines.contains_key(&req.profile) {
            return Err(SaveError::ProfileNotFound);
        }
        let blob_created = st.insert_blob(req.blob.sha256, req.blob.size);
        let record = ParkedRecord {
            id: Uuid::now_v7(),
            request: req.clone(),
            parked_at: now,
            expires_at: now.plus_millis(req.retain_ms),
            blob_created,
        };
        st.parked.push(record.clone());
        Ok(record)
    }

    /// See [`SaveStore::parked`] (expired bundles included until a test drops them).
    pub fn parked(&self, profile: ProfileId) -> Result<Vec<ParkedRecord>, SaveError> {
        let st = self.state.lock().map_err(poisoned)?;
        Ok(st
            .parked
            .iter()
            .filter(|p| p.request.profile == profile)
            .cloned()
            .collect())
    }

    /// Test control: makes the line's lease lapse now, as if its heartbeats had stopped.
    pub fn expire_lease(&self, profile: ProfileId, now: UnixMs) -> Result<(), SaveError> {
        let mut st = self.state.lock().map_err(poisoned)?;
        let line = st
            .lines
            .get_mut(&profile)
            .ok_or(SaveError::ProfileNotFound)?;
        if let Some(lease) = line.lease.as_mut() {
            lease.expires_at = now.plus_millis(-1);
            lease.accounted_at = lease.accounted_at.min(lease.expires_at);
        }
        Ok(())
    }
}

/// [`MemoryCore`] behind the async trait, with a clock. Cloning shares the store.
#[derive(Clone)]
pub struct MemorySaveStore {
    core: Arc<MemoryCore>,
    clock: Arc<dyn Clock>,
}

impl MemorySaveStore {
    /// An empty store reading `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            core: Arc::new(MemoryCore::new()),
            clock,
        }
    }

    /// The synchronous core (parked bundles, test controls).
    pub fn core(&self) -> &MemoryCore {
        &self.core
    }

    /// See [`MemoryCore::ensure_line_as`].
    pub fn ensure_line_as(&self, profile: ProfileId, mode: ProfileMode) -> Result<(), SaveError> {
        self.core.ensure_line_as(profile, mode)
    }

    /// Test control: see [`MemoryCore::expire_lease`].
    pub fn expire_lease(&self, profile: ProfileId) -> Result<(), SaveError> {
        self.core.expire_lease(profile, self.clock.now())
    }
}

impl SaveStore for MemorySaveStore {
    async fn ensure_line(&self, profile: ProfileId) -> Result<(), SaveError> {
        self.core.ensure_line(profile)
    }

    async fn head(&self, profile: ProfileId) -> Result<HeadView, SaveError> {
        self.core.head(profile, self.clock.now())
    }

    async fn acquire(
        &self,
        profile: ProfileId,
        req: AcquireRequest,
    ) -> Result<LeaseGrant, SaveError> {
        self.core.acquire(profile, &req, self.clock.now())
    }

    async fn renew(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
        ttl_ms: u32,
    ) -> Result<Lease, SaveError> {
        self.core
            .renew(profile, session, token, ttl_ms, self.clock.now())
    }

    async fn release(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        token: FenceToken,
    ) -> Result<Released, SaveError> {
        self.core.release(profile, session, token, self.clock.now())
    }

    async fn commit(&self, req: CommitRequest) -> Result<Committed, SaveError> {
        self.core.commit(&req, self.clock.now())
    }

    async fn history(
        &self,
        profile: ProfileId,
        before: Option<Revision>,
        limit: u32,
    ) -> Result<Vec<RevisionRecord>, SaveError> {
        self.core.history(profile, before, limit)
    }

    async fn play_window_start(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
        min_play_ms: u64,
    ) -> Result<Option<RevisionRecord>, SaveError> {
        self.core
            .play_window_start(profile, at_or_below, min_play_ms)
    }

    async fn level_high_water(
        &self,
        profile: ProfileId,
        at_or_below: Revision,
    ) -> Result<Option<i64>, SaveError> {
        self.core.level_high_water(profile, at_or_below)
    }

    async fn revision(
        &self,
        profile: ProfileId,
        revision: Revision,
    ) -> Result<RevisionRecord, SaveError> {
        self.core.revision(profile, revision)
    }

    async fn find_commit(&self, commit_id: CommitId) -> Result<Option<RevisionRecord>, SaveError> {
        self.core.find_commit(commit_id)
    }

    async fn park(&self, req: ParkRequest) -> Result<ParkedRecord, SaveError> {
        self.core.park(&req, self.clock.now())
    }

    async fn parked(&self, profile: ProfileId) -> Result<Vec<ParkedRecord>, SaveError> {
        self.core.parked(profile)
    }
}
