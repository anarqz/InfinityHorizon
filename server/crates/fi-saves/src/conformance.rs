//! Store conformance suite (feature `testkit`): the scenarios every [`SaveStore`] must pass,
//! run against the in-memory store (tests/memory_conformance.rs) and against Postgres
//! (crates/fi-db/tests/pg_saves.rs). CLOUD_SAVES.md §11 maps each scenario to the Beta failure
//! it rules out.
//!
//! The concurrent scenarios spawn real tasks on a multi-thread runtime; against Postgres each
//! task holds its own connection, so the interleavings are decided by the database's row locks,
//! exactly as in production. The last scenario runs the real [`SaveService`] over the store with
//! a blob store that blocks one upload mid-put, to replay the Hardcore gate race the 2026-10-17
//! review found (docs/server/REVIEW_LOG.md S1).

use crate::blob::{BlobStore, MemoryBlobStore};
use crate::bundle;
use crate::error::SaveError;
use crate::model::{
    AcquireMode, AcquireRequest, Actor, BlobRef, CommitId, CommitKind, CommitRequest, Digest,
    Lease, LeaseHolder, ParkReason, ParkRequest, Released, Replaced, Revision, SaveSummary,
};
use crate::service::{RestoreRequest, SavePolicy, SaveService, UploadRequest};
use crate::store::SaveStore;
use fi_domain::{DeviceId, PlaySessionId, ProfileId, ProfileMode};
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio::task::JoinSet;

/// A synthetic fresh career ([`crate::synthetic::starter`]): made-up bytes in the FH2SAVE1 shape.
pub fn starter() -> &'static [u8] {
    crate::synthetic::starter()
}

/// The starter save with other credits, repacked under `label` (a new Azimuth revision folder).
pub fn save_with_credits(credits: i32, label: u64) -> Vec<u8> {
    let mut files = bundle::validate(starter()).expect("starter").files;
    let i = files
        .iter()
        .position(|(n, _)| *n == "data/ProfileData")
        .expect("ProfileData");
    files[i].1 = bundle::set_profile_int(&files[i].1, "Credits", credits).expect("Credits");
    let data: Vec<(&str, Vec<u8>)> = files
        .iter()
        .filter(|(n, _)| *n != "commit")
        .map(|(n, b)| (*n, b.clone()))
        .collect();
    files[0].1 = bundle::commit_with(&files[0].1, &data);
    let owned: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, b)| (*n, b.clone())).collect();
    bundle::pack(label, &owned)
}

/// What the suite needs from the store under test.
pub trait Harness: Send + Sync {
    /// The store type.
    type Store: SaveStore + Clone + 'static;

    /// A handle to the store (shared state).
    fn store(&self) -> Self::Store;

    /// A new Noob profile with an empty save line.
    fn new_profile(&self) -> impl Future<Output = ProfileId> + Send {
        self.new_profile_as(ProfileMode::Noob)
    }

    /// A new profile of `mode` with an empty save line.
    fn new_profile_as(&self, mode: ProfileMode) -> impl Future<Output = ProfileId> + Send;

    /// Makes the line's lease lapse now, as if its heartbeats had stopped.
    fn expire_lease(&self, profile: ProfileId) -> impl Future<Output = ()> + Send;

    /// Lets `ms` milliseconds of store time pass (memory: moves its clock; Postgres: sleeps,
    /// the database clock moves on its own).
    fn advance(&self, ms: u64) -> impl Future<Output = ()> + Send;
}

/// TTL used by the scenarios: long enough that nothing lapses unless a scenario expires it.
pub const TTL_MS: u32 = 600_000;

/// A holder on a fresh session.
pub fn holder(device: DeviceId) -> LeaseHolder {
    LeaseHolder {
        session: PlaySessionId::new(),
        device,
        device_label: format!("PC-{}", &device.to_string()[..8]),
    }
}

/// An acquire request.
pub fn acquire(holder: &LeaseHolder, mode: AcquireMode) -> AcquireRequest {
    AcquireRequest {
        holder: holder.clone(),
        ttl_ms: TTL_MS,
        mode,
    }
}

/// A commit of a synthetic bundle (stores never read blob bytes) named by `tag`.
pub fn commit(profile: ProfileId, lease: &Lease, expected: Revision, tag: &str) -> CommitRequest {
    CommitRequest {
        profile,
        session: lease.holder.session,
        token: lease.token,
        expected,
        commit_id: CommitId::new(),
        blob: BlobRef {
            sha256: Digest::of(tag.as_bytes()),
            size: 28_000,
            format: None,
        },
        content: Digest::of(format!("content:{tag}").as_bytes()),
        kind: CommitKind::Upload,
        actor: Actor::Player,
        device: Some(lease.holder.device),
        client_label: Some("00000000000000A1".into()),
        restored_from: None,
        summary: SaveSummary {
            credits: Some(1_000),
            xp: Some(2_000),
            level: Some(3),
            ..SaveSummary::default()
        },
        gated_base: None,
        parked_from: None,
        guard_input: None,
        seed_source: None,
    }
}

/// A system commit of `kind` (fresh career, resets) by `actor`, on `expected`.
pub fn system_commit(
    profile: ProfileId,
    lease: &Lease,
    expected: Revision,
    kind: CommitKind,
    actor: Actor,
    tag: &str,
) -> CommitRequest {
    CommitRequest {
        kind,
        actor,
        device: None,
        client_label: None,
        ..commit(profile, lease, expected, tag)
    }
}

async fn grant<S: SaveStore>(
    store: &S,
    profile: ProfileId,
    holder: &LeaseHolder,
    mode: AcquireMode,
) -> Lease {
    store
        .acquire(profile, acquire(holder, mode))
        .await
        .expect("acquire")
        .lease
}

/// Runs every scenario, printing each name so a failure is easy to place.
pub async fn run_all<H: Harness>(h: &H) {
    macro_rules! scenario {
        ($name:ident) => {
            println!("conformance: {}", stringify!($name));
            $name(h).await;
        };
    }
    scenario!(unknown_profile_is_not_found);
    scenario!(empty_line_has_no_head);
    scenario!(tokens_strictly_increase);
    scenario!(live_lease_of_another_device_is_held);
    scenario!(retried_acquire_is_idempotent);
    scenario!(same_device_replaces_a_crashed_session);
    scenario!(takeover_fences_the_previous_holder);
    scenario!(stale_expected_revision_is_a_conflict);
    scenario!(commits_are_idempotent);
    scenario!(idempotency_key_reuse_is_refused);
    scenario!(expired_lease_must_be_reacquired);
    scenario!(expired_lease_taken_elsewhere_fences_the_old_holder);
    scenario!(release_frees_the_line);
    scenario!(renew_and_commit_extend_the_lease);
    scenario!(history_is_contiguous_newest_first);
    scenario!(parked_bundles_are_kept);
    scenario!(concurrent_acquires_have_exactly_one_winner);
    scenario!(concurrent_commits_never_lose_an_update);
    scenario!(concurrent_takeovers_leave_exactly_one_writer);
    scenario!(concurrent_duplicate_commits_commit_once);
    scenario!(hardcore_uploads_land_only_on_their_gated_head);
    scenario!(lines_start_by_their_mode);
    scenario!(resets_bind_player_restores);
    scenario!(resume_reacquires_only_a_line_nobody_took);
    scenario!(play_time_accrues_only_while_leased);
    scenario!(the_rate_window_starts_after_the_newest_anchor);
    scenario!(replays_are_found_by_their_key);
    scenario!(writes_report_the_blob_rows_they_create);
    scenario!(the_gates_cannot_be_raced);
}

/// Every operation on an unknown profile answers `PROFILE_NOT_FOUND`.
pub async fn unknown_profile_is_not_found<H: Harness>(h: &H) {
    let store = h.store();
    let ghost = ProfileId::new();
    assert_eq!(
        store.head(ghost).await.unwrap_err(),
        SaveError::ProfileNotFound
    );
    let a = holder(DeviceId::new());
    assert_eq!(
        store
            .acquire(ghost, acquire(&a, AcquireMode::Normal))
            .await
            .unwrap_err(),
        SaveError::ProfileNotFound
    );
}

/// A new line is at revision 0 with no lease.
pub async fn empty_line_has_no_head<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    store
        .ensure_line(p)
        .await
        .expect("ensure_line is idempotent");
    let head = store.head(p).await.unwrap();
    assert_eq!(head.revision, Revision::NONE);
    assert!(head.current.is_none());
    assert!(head.lease.is_none());
    assert_eq!(head.fence_seq, 0);
    assert!(store.history(p, None, 10).await.unwrap().is_empty());
}

/// Each grant's token is one more than the last, whatever replaced what.
pub async fn tokens_strictly_increase<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let first = grant(&store, p, &a, AcquireMode::Normal).await;
    assert_eq!(first.token.get(), 1);
    assert_eq!(
        store.release(p, a.session, first.token).await.unwrap(),
        Released::Released
    );
    let b = holder(DeviceId::new());
    let second = grant(&store, p, &b, AcquireMode::Normal).await;
    let c = holder(DeviceId::new());
    let third = grant(&store, p, &c, AcquireMode::Takeover).await;
    assert_eq!((second.token.get(), third.token.get()), (2, 3));
    assert_eq!(store.head(p).await.unwrap().fence_seq, 3);
}

/// A live lease of another device refuses a normal acquire and names the holder.
pub async fn live_lease_of_another_device_is_held<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let lease = grant(&store, p, &a, AcquireMode::Normal).await;
    let b = holder(DeviceId::new());
    match store.acquire(p, acquire(&b, AcquireMode::Normal)).await {
        Err(SaveError::LeaseHeld {
            device,
            device_label,
            expires_at,
        }) => {
            assert_eq!(device, a.device);
            assert_eq!(device_label, a.device_label);
            assert_eq!(expires_at, lease.expires_at);
        }
        other => panic!("expected LEASE_HELD, got {other:?}"),
    }
}

/// The same session acquiring again (a retry) gets the same lease back.
pub async fn retried_acquire_is_idempotent<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let first = store
        .acquire(p, acquire(&a, AcquireMode::Normal))
        .await
        .unwrap();
    let again = store
        .acquire(p, acquire(&a, AcquireMode::Normal))
        .await
        .unwrap();
    assert!(!first.existing && again.existing);
    assert_eq!(first.lease.token, again.lease.token);
    assert_eq!(store.head(p).await.unwrap().fence_seq, 1);
}

/// A new session on the device that holds the lease replaces it without a prompt.
pub async fn same_device_replaces_a_crashed_session<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let device = DeviceId::new();
    let crashed = holder(device);
    let old = grant(&store, p, &crashed, AcquireMode::Normal).await;
    let restarted = holder(device);
    let g = store
        .acquire(p, acquire(&restarted, AcquireMode::Normal))
        .await
        .unwrap();
    assert!(matches!(g.replaced, Some(Replaced::SameDevice(ref l)) if l.token == old.token));
    assert!(g.lease.token > old.token);
    let late = store.commit(commit(p, &old, Revision::NONE, "late")).await;
    assert!(
        matches!(late, Err(SaveError::LeaseLost { .. })),
        "the crashed session is fenced: {late:?}"
    );
}

/// After a takeover, the previous holder can never write again; the new holder can.
pub async fn takeover_fences_the_previous_holder<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let la = grant(&store, p, &a, AcquireMode::Normal).await;
    store
        .commit(commit(p, &la, Revision::NONE, "a1"))
        .await
        .unwrap();
    let b = holder(DeviceId::new());
    let g = store
        .acquire(p, acquire(&b, AcquireMode::Takeover))
        .await
        .unwrap();
    assert!(matches!(g.replaced, Some(Replaced::TakenOver(_))));
    let fenced = store.commit(commit(p, &la, Revision::new(1), "a2")).await;
    assert_eq!(
        fenced.unwrap_err(),
        SaveError::LeaseLost {
            current: Some(g.lease.token)
        }
    );
    let ok = store
        .commit(commit(p, &g.lease, Revision::new(1), "b2"))
        .await
        .unwrap();
    assert_eq!(ok.record.revision, Revision::new(2));
    assert!(matches!(
        store.renew(p, a.session, la.token, TTL_MS).await,
        Err(SaveError::LeaseLost { .. })
    ));
}

/// A commit based on an old head is refused with the current head.
pub async fn stale_expected_revision_is_a_conflict<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    store
        .commit(commit(p, &l, Revision::NONE, "r1"))
        .await
        .unwrap();
    let stale = store
        .commit(commit(p, &l, Revision::NONE, "r1-again"))
        .await;
    assert_eq!(
        stale.unwrap_err(),
        SaveError::RevisionConflict {
            expected: Revision::NONE,
            head: Revision::new(1)
        }
    );
    let ahead = store.commit(commit(p, &l, Revision::new(5), "ahead")).await;
    assert_eq!(
        ahead.unwrap_err(),
        SaveError::RevisionConflict {
            expected: Revision::new(5),
            head: Revision::new(1)
        }
    );
}

/// Retrying a commit with its idempotency key returns the first result and writes nothing.
pub async fn commits_are_idempotent<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    let req = commit(p, &l, Revision::NONE, "once");
    let first = store.commit(req.clone()).await.unwrap();
    let retry = store.commit(req).await.unwrap();
    assert!(!first.replayed && retry.replayed);
    assert_eq!(first.record, retry.record);
    assert_eq!(store.head(p).await.unwrap().revision, Revision::new(1));
    assert_eq!(store.history(p, None, 10).await.unwrap().len(), 1);
}

/// An idempotency key that committed one bundle cannot commit another.
pub async fn idempotency_key_reuse_is_refused<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    let first = commit(p, &l, Revision::NONE, "first");
    store.commit(first.clone()).await.unwrap();
    let mut other = commit(p, &l, Revision::new(1), "other");
    other.commit_id = first.commit_id;
    assert_eq!(
        store.commit(other).await.unwrap_err(),
        SaveError::IdempotencyKeyReused
    );
}

/// A lapsed lease cannot commit; re-acquiring gives a new token and the same commit then lands.
pub async fn expired_lease_must_be_reacquired<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    h.expire_lease(p).await;
    let lapsed = store.commit(commit(p, &l, Revision::NONE, "late")).await;
    assert!(
        matches!(lapsed, Err(SaveError::LeaseExpired { .. })),
        "{lapsed:?}"
    );
    assert!(matches!(
        store.renew(p, a.session, l.token, TTL_MS).await,
        Err(SaveError::LeaseExpired { .. })
    ));
    let g = store
        .acquire(p, acquire(&a, AcquireMode::Normal))
        .await
        .unwrap();
    assert!(matches!(g.replaced, Some(Replaced::Expired(_))));
    assert!(g.lease.token > l.token);
    let ok = store
        .commit(commit(p, &g.lease, Revision::NONE, "late"))
        .await
        .unwrap();
    assert_eq!(ok.record.revision, Revision::new(1));
}

/// A lapsed lease taken by another device fences the old holder, even with the right head.
pub async fn expired_lease_taken_elsewhere_fences_the_old_holder<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let la = grant(&store, p, &a, AcquireMode::Normal).await;
    h.expire_lease(p).await;
    let b = holder(DeviceId::new());
    let lb = grant(&store, p, &b, AcquireMode::Normal).await;
    let fenced = store.commit(commit(p, &la, Revision::NONE, "a")).await;
    assert_eq!(
        fenced.unwrap_err(),
        SaveError::LeaseLost {
            current: Some(lb.token)
        }
    );
    store
        .commit(commit(p, &lb, Revision::NONE, "b"))
        .await
        .unwrap();
}

/// Release frees the line for a normal acquire; releasing twice is harmless; releasing someone
/// else's lease is refused and changes nothing.
pub async fn release_frees_the_line<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let la = grant(&store, p, &a, AcquireMode::Normal).await;
    assert_eq!(
        store.release(p, a.session, la.token).await.unwrap(),
        Released::Released
    );
    assert_eq!(
        store.release(p, a.session, la.token).await.unwrap(),
        Released::AlreadyReleased
    );
    assert!(store.head(p).await.unwrap().lease.is_none());
    let b = holder(DeviceId::new());
    let lb = grant(&store, p, &b, AcquireMode::Normal).await;
    assert!(matches!(
        store.release(p, a.session, la.token).await,
        Err(SaveError::LeaseLost { .. })
    ));
    assert_eq!(
        store.head(p).await.unwrap().lease.map(|l| l.token),
        Some(lb.token)
    );
}

/// Heartbeats and commits push the expiry forward.
pub async fn renew_and_commit_extend_the_lease<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    let renewed = store.renew(p, a.session, l.token, TTL_MS).await.unwrap();
    assert!(renewed.expires_at >= l.expires_at);
    assert_eq!(renewed.token, l.token);
    store
        .commit(commit(p, &renewed, Revision::NONE, "r1"))
        .await
        .unwrap();
    let head = store.head(p).await.unwrap();
    assert!(head.lease.as_ref().unwrap().expires_at >= renewed.expires_at);
    assert!(head.live_lease().is_some());
}

/// History lists every revision once, newest first, with `before` paging.
pub async fn history_is_contiguous_newest_first<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    for i in 0..5u64 {
        let c = store
            .commit(commit(p, &l, Revision::new(i), &format!("h{i}")))
            .await
            .unwrap();
        assert_eq!(c.record.revision, Revision::new(i + 1));
        assert_eq!(c.record.token, l.token);
    }
    let all: Vec<u64> = store
        .history(p, None, 10)
        .await
        .unwrap()
        .iter()
        .map(|r| r.revision.get())
        .collect();
    assert_eq!(all, [5, 4, 3, 2, 1]);
    let page: Vec<u64> = store
        .history(p, Some(Revision::new(3)), 10)
        .await
        .unwrap()
        .iter()
        .map(|r| r.revision.get())
        .collect();
    assert_eq!(page, [2, 1]);
    let r3 = store.revision(p, Revision::new(3)).await.unwrap();
    assert_eq!(r3.blob.sha256, Digest::of(b"h2"));
    assert_eq!(r3.summary.level, Some(3));
    assert_eq!(
        store.revision(p, Revision::new(9)).await.unwrap_err(),
        SaveError::RevisionNotFound(Revision::new(9))
    );
    let head = store.head(p).await.unwrap();
    assert_eq!(head.current.map(|r| r.revision), Some(Revision::new(5)));
}

/// Parked bundles are stored with their reason and an expiry.
pub async fn parked_bundles_are_kept<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let parked = store
        .park(ParkRequest {
            profile: p,
            blob: BlobRef {
                sha256: Digest::of(b"loser"),
                size: 27_000,
                format: None,
            },
            content: Digest::of(b"content:loser"),
            reason: ParkReason::ConflictLoser,
            based_on: Some(Revision::NONE),
            session: None,
            device: None,
            summary: None,
            detail: "kept the cloud save".into(),
            retain_ms: 86_400_000,
        })
        .await
        .unwrap();
    assert_eq!(parked.request.reason, ParkReason::ConflictLoser);
    assert_eq!(parked.expires_at.since(parked.parked_at), 86_400_000);
    assert_eq!(
        store.head(p).await.unwrap().revision,
        Revision::NONE,
        "parking never touches the line"
    );
}

/// N sessions on N devices race for a free line: exactly one gets it.
pub async fn concurrent_acquires_have_exactly_one_winner<H: Harness>(h: &H) {
    const N: usize = 16;
    let store = h.store();
    let p = h.new_profile().await;
    let mut tasks = JoinSet::new();
    for _ in 0..N {
        let store = store.clone();
        tasks.spawn(async move {
            store
                .acquire(p, acquire(&holder(DeviceId::new()), AcquireMode::Normal))
                .await
        });
    }
    let (mut won, mut held) = (Vec::new(), 0);
    while let Some(joined) = tasks.join_next().await {
        match joined.expect("task") {
            Ok(g) => won.push(g.lease.token),
            Err(SaveError::LeaseHeld { .. }) => held += 1,
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(won.len(), 1, "exactly one winner, got {won:?}");
    assert_eq!(held, N - 1);
    assert_eq!(store.head(p).await.unwrap().fence_seq, won[0].get());
}

/// M tasks of the lease holder each land K commits, re-reading the head on conflict. Every
/// accepted commit is in history exactly once and revisions are 1..=M*K with no gap or repeat:
/// no update is lost and no two commits share a base.
pub async fn concurrent_commits_never_lose_an_update<H: Harness>(h: &H) {
    const M: usize = 8;
    const K: usize = 10;
    let store = h.store();
    let p = h.new_profile().await;
    let lease = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    let mut tasks = JoinSet::new();
    for m in 0..M {
        let store = store.clone();
        let lease = lease.clone();
        tasks.spawn(async move {
            let mut landed = Vec::new();
            for k in 0..K {
                let mut attempts = 0;
                loop {
                    attempts += 1;
                    assert!(attempts < 10_000, "task {m} starved");
                    let head = store.head(p).await.expect("head").revision;
                    match store
                        .commit(commit(p, &lease, head, &format!("m{m}k{k}a{attempts}")))
                        .await
                    {
                        Ok(c) => {
                            assert!(!c.replayed);
                            assert_eq!(
                                c.record.revision,
                                head.next(),
                                "a commit lands exactly on its base"
                            );
                            landed.push((c.record.revision, c.record.commit_id));
                            break;
                        }
                        Err(SaveError::RevisionConflict { .. }) => continue,
                        Err(other) => panic!("unexpected {other:?}"),
                    }
                }
            }
            landed
        });
    }
    let mut landed = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        landed.extend(joined.expect("task"));
    }
    let total = (M * K) as u64;
    assert_eq!(landed.len() as u64, total);
    let revisions: HashSet<u64> = landed.iter().map(|(r, _)| r.get()).collect();
    assert_eq!(
        revisions,
        (1..=total).collect(),
        "every revision once, no gap"
    );
    let head = store.head(p).await.unwrap();
    assert_eq!(head.revision.get(), total);
    let history = store.history(p, None, (total + 10) as u32).await.unwrap();
    assert_eq!(history.len() as u64, total);
    for (i, r) in history.iter().enumerate() {
        assert_eq!(
            r.revision.get(),
            total - i as u64,
            "newest first and contiguous"
        );
    }
    let in_history: HashSet<CommitId> = history.iter().map(|r| r.commit_id).collect();
    let accepted: HashSet<CommitId> = landed.iter().map(|(_, c)| *c).collect();
    assert_eq!(in_history, accepted, "every accepted commit is in history");
}

/// N devices take over at once: tokens are all distinct, the last grant holds the line, and when
/// all of them then try to write, exactly one write lands. No double writer.
pub async fn concurrent_takeovers_leave_exactly_one_writer<H: Harness>(h: &H) {
    const N: usize = 12;
    let store = h.store();
    let p = h.new_profile().await;
    let mut tasks = JoinSet::new();
    for _ in 0..N {
        let store = store.clone();
        tasks.spawn(async move {
            store
                .acquire(p, acquire(&holder(DeviceId::new()), AcquireMode::Takeover))
                .await
                .expect("takeover")
                .lease
        });
    }
    let mut leases = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        leases.push(joined.expect("task"));
    }
    let tokens: HashSet<u64> = leases.iter().map(|l| l.token.get()).collect();
    assert_eq!(tokens.len(), N, "every grant has its own token");
    let max = *tokens.iter().max().unwrap();
    let head = store.head(p).await.unwrap();
    assert_eq!(
        head.lease.as_ref().map(|l| l.token.get()),
        Some(max),
        "the newest grant holds the line"
    );

    let mut writes = JoinSet::new();
    for lease in leases {
        let store = store.clone();
        writes.spawn(async move {
            (
                lease.token,
                store
                    .commit(commit(
                        p,
                        &lease,
                        Revision::NONE,
                        &format!("w{}", lease.token),
                    ))
                    .await,
            )
        });
    }
    let mut ok = Vec::new();
    while let Some(joined) = writes.join_next().await {
        let (token, result) = joined.expect("task");
        match result {
            Ok(c) => ok.push((token, c.record.revision)),
            Err(SaveError::LeaseLost { .. }) => {}
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(ok.len(), 1, "exactly one writer: {ok:?}");
    assert_eq!(ok[0].0.get(), max, "and it is the current holder");
    assert_eq!(store.head(p).await.unwrap().revision, Revision::new(1));
}

/// K copies of the same commit race (a client retrying while the first is in flight): one write.
pub async fn concurrent_duplicate_commits_commit_once<H: Harness>(h: &H) {
    const K: usize = 10;
    let store = h.store();
    let p = h.new_profile().await;
    let lease = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    let req = commit(p, &lease, Revision::NONE, "dup");
    let mut tasks = JoinSet::new();
    for _ in 0..K {
        let store = store.clone();
        let req = req.clone();
        tasks.spawn(async move { store.commit(req).await });
    }
    let mut fresh = 0;
    while let Some(joined) = tasks.join_next().await {
        let c = joined.expect("task").expect("every copy succeeds");
        assert_eq!(c.record.revision, Revision::new(1));
        if !c.replayed {
            fresh += 1;
        }
    }
    assert_eq!(fresh, 1, "one copy wrote, the others replayed it");
    assert_eq!(store.history(p, None, 10).await.unwrap().len(), 1);
}

async fn fresh_career<S: SaveStore>(store: &S, p: ProfileId, lease: &Lease) -> Lease {
    let v = bundle::verify(starter()).expect("starter");
    let c = store
        .commit(CommitRequest {
            blob: BlobRef {
                sha256: Digest::of(starter()),
                size: starter().len() as u32,
                format: None,
            },
            content: v.content,
            summary: v.summary,
            ..system_commit(
                p,
                lease,
                Revision::NONE,
                CommitKind::FreshCareer,
                Actor::System,
                "fresh",
            )
        })
        .await
        .expect("fresh career");
    assert_eq!(c.record.revision, Revision::new(1));
    store.head(p).await.unwrap().lease.expect("lease")
}

/// A Hardcore upload lands only with `gated_base` equal to the head it lands on: ungated, gated
/// against another head, or aimed one ahead of the head, it never lands (review S1).
pub async fn hardcore_uploads_land_only_on_their_gated_head<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile_as(ProfileMode::Hardcore).await;
    let l = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    let l = fresh_career(&store, p, &l).await;
    let ungated = store
        .commit(commit(p, &l, Revision::new(1), "ungated"))
        .await;
    assert!(
        matches!(ungated, Err(SaveError::InvalidArgument(_))),
        "{ungated:?}"
    );
    let mut stale = commit(p, &l, Revision::new(1), "stale-verdict");
    stale.gated_base = Some(Revision::NONE);
    assert!(matches!(
        store.commit(stale).await,
        Err(SaveError::InvalidArgument(_))
    ));
    let mut ahead = commit(p, &l, Revision::new(2), "ahead");
    ahead.gated_base = Some(Revision::new(2));
    assert_eq!(
        store.commit(ahead).await.unwrap_err(),
        SaveError::RevisionConflict {
            expected: Revision::new(2),
            head: Revision::new(1)
        }
    );
    let mut gated = commit(p, &l, Revision::new(1), "gated");
    gated.gated_base = Some(Revision::new(1));
    assert_eq!(
        store.commit(gated).await.unwrap().record.revision,
        Revision::new(2)
    );
    assert_eq!(store.head(p).await.unwrap().revision, Revision::new(2));
    assert_eq!(store.history(p, None, 10).await.unwrap().len(), 2);
}

/// A Hardcore line starts with the server's fresh career and never from an upload or an offline
/// import; an offline import starts only an empty Noob line.
pub async fn lines_start_by_their_mode<H: Harness>(h: &H) {
    let store = h.store();
    let hard = h.new_profile_as(ProfileMode::Hardcore).await;
    let lh = grant(&store, hard, &holder(DeviceId::new()), AcquireMode::Normal).await;
    assert_eq!(store.head(hard).await.unwrap().mode, ProfileMode::Hardcore);
    let mut import = commit(hard, &lh, Revision::NONE, "import");
    import.kind = CommitKind::ImportOffline;
    assert_eq!(
        store.commit(import).await.unwrap_err(),
        SaveError::HardcoreNeedsFreshCareer
    );
    assert_eq!(
        store
            .commit(commit(hard, &lh, Revision::NONE, "first"))
            .await
            .unwrap_err(),
        SaveError::HardcoreNeedsFreshCareer
    );
    let forged = system_commit(
        hard,
        &lh,
        Revision::NONE,
        CommitKind::FreshCareer,
        Actor::Player,
        "forged",
    );
    assert!(matches!(
        store.commit(forged).await,
        Err(SaveError::InvalidArgument(_))
    ));
    fresh_career(&store, hard, &lh).await;

    let noob = h.new_profile().await;
    let ln = grant(&store, noob, &holder(DeviceId::new()), AcquireMode::Normal).await;
    let not_noob = system_commit(
        noob,
        &ln,
        Revision::NONE,
        CommitKind::FreshCareer,
        Actor::System,
        "noob-fresh",
    );
    assert!(matches!(
        store.commit(not_noob).await,
        Err(SaveError::InvalidArgument(_))
    ));
    let mut first = commit(noob, &ln, Revision::NONE, "offline");
    first.kind = CommitKind::ImportOffline;
    store.commit(first).await.unwrap();
    let mut second = commit(noob, &ln, Revision::new(1), "offline-again");
    second.kind = CommitKind::ImportOffline;
    assert!(matches!(
        store.commit(second).await,
        Err(SaveError::InvalidArgument(_))
    ));
}

/// A reset moves the line's reset revision: a player can no longer restore anything older, a
/// moderator still can.
pub async fn resets_bind_player_restores<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let l = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    for i in 0..2u64 {
        store
            .commit(commit(p, &l, Revision::new(i), &format!("pre{i}")))
            .await
            .unwrap();
    }
    let reset = store
        .commit(system_commit(
            p,
            &l,
            Revision::new(2),
            CommitKind::SeasonReset,
            Actor::System,
            "season",
        ))
        .await
        .unwrap();
    assert_eq!(reset.record.revision, Revision::new(3));
    assert_eq!(
        store.head(p).await.unwrap().reset_revision,
        Revision::new(3)
    );
    let restore = |from: u64, expected: u64, actor: Actor, tag: &str| CommitRequest {
        kind: CommitKind::Restore,
        actor,
        restored_from: Some(Revision::new(from)),
        device: None,
        client_label: None,
        ..commit(p, &l, Revision::new(expected), tag)
    };
    assert_eq!(
        store
            .commit(restore(2, 3, Actor::Player, "undo"))
            .await
            .unwrap_err(),
        SaveError::RestoreBehindReset {
            reset: Revision::new(3)
        }
    );
    let by_support = store
        .commit(restore(2, 3, Actor::Moderator, "support"))
        .await
        .unwrap();
    assert_eq!(by_support.record.revision, Revision::new(4));
    assert_eq!(
        store.head(p).await.unwrap().reset_revision,
        Revision::new(3),
        "a restore is not a reset"
    );
    store
        .commit(restore(3, 4, Actor::Player, "back-to-reset"))
        .await
        .expect("the reset and anything after it can be restored");
}

/// A heartbeat that finds its lease lapsed gets a new token only if nobody was granted the line
/// since; taken or released, it is `LEASE_LOST` (SAVES_API.md §3.2).
pub async fn resume_reacquires_only_a_line_nobody_took<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let la = grant(&store, p, &a, AcquireMode::Normal).await;
    let resume = |previous| AcquireRequest {
        holder: a.clone(),
        ttl_ms: TTL_MS,
        mode: AcquireMode::Resume { previous },
    };
    let live = store.acquire(p, resume(la.token)).await.unwrap();
    assert!(live.existing, "a live lease: a retry");
    h.expire_lease(p).await;
    let back = store.acquire(p, resume(la.token)).await.unwrap();
    assert!(!back.existing && back.lease.token > la.token);
    assert!(matches!(back.replaced, Some(Replaced::Expired(_))));
    assert!(matches!(
        store
            .commit(commit(p, &la, Revision::NONE, "old-token"))
            .await,
        Err(SaveError::LeaseLost { .. })
    ));
    store
        .commit(commit(p, &back.lease, Revision::NONE, "resumed"))
        .await
        .expect("the resumed token writes");
    h.expire_lease(p).await;
    let b = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    assert_eq!(
        store
            .acquire(p, resume(back.lease.token))
            .await
            .unwrap_err(),
        SaveError::LeaseLost {
            current: Some(b.token)
        },
        "someone took the line after the lapse"
    );
    store.release(p, b.holder.session, b.token).await.unwrap();
    assert_eq!(
        store.acquire(p, resume(b.token)).await.unwrap_err(),
        SaveError::LeaseLost { current: None },
        "a released lease is never resumed: the session ended"
    );
}

/// The guardrails' rate window (CLOUD_SAVES.md §7.3) starts at the newest readable revision below
/// the window's play time (its step into the window counts, review G2-5), else the oldest readable
/// one (review G2-12), never before the newest rate anchor; a restore is not one (review G2-2).
/// The spin allowance's level high-water mark restarts at the newest anchor or restore (G2-3).
pub async fn the_rate_window_starts_after_the_newest_anchor<H: Harness>(h: &H) {
    let store = h.store();
    assert_eq!(
        store
            .play_window_start(ProfileId::new(), Revision::new(9), 0)
            .await
            .unwrap(),
        None,
        "no line"
    );
    assert_eq!(
        store
            .level_high_water(ProfileId::new(), Revision::new(9))
            .await
            .unwrap(),
        None,
        "no line"
    );
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let mut l = grant(&store, p, &a, AcquireMode::Normal).await;
    let mut play = Vec::new();
    // Revision 1 unreadable, 4 a season reset, 7 a restore; the levels of the others.
    let levels = [
        None,
        Some(10),
        Some(4),
        Some(1),
        Some(7),
        Some(9),
        Some(5),
        Some(6),
    ];
    for (i, level) in levels.into_iter().enumerate() {
        let i = i as u64;
        h.advance(60).await;
        l = store.renew(p, a.session, l.token, TTL_MS).await.unwrap();
        let mut req = match i {
            3 => system_commit(
                p,
                &l,
                Revision::new(i),
                CommitKind::SeasonReset,
                Actor::System,
                "window-reset",
            ),
            6 => CommitRequest {
                restored_from: Some(Revision::new(5)),
                ..system_commit(
                    p,
                    &l,
                    Revision::new(i),
                    CommitKind::Restore,
                    Actor::Moderator,
                    "window-restore",
                )
            },
            _ => commit(p, &l, Revision::new(i), &format!("window{i}")),
        };
        req.summary = match level {
            Some(level) => SaveSummary {
                level: Some(level),
                ..req.summary
            },
            None => SaveSummary::default(),
        };
        play.push(store.commit(req).await.unwrap().record.play_ms);
    }
    assert!(play.windows(2).all(|w| w[0] < w[1]), "{play:?}");
    let start = |at: u64, min: u64| {
        let store = store.clone();
        async move {
            store
                .play_window_start(p, Revision::new(at), min)
                .await
                .unwrap()
                .map(|r| r.revision.get())
        }
    };
    assert_eq!(start(3, 0).await, Some(2), "the oldest readable revision");
    assert_eq!(
        start(3, play[2]).await,
        Some(2),
        "the newest readable revision below the window"
    );
    assert_eq!(
        start(3, play[1]).await,
        Some(2),
        "none readable below the window: the oldest readable"
    );
    assert_eq!(start(8, 0).await, Some(4), "the reset is the anchor");
    assert_eq!(start(8, play[6]).await, Some(6), "the restore is not");
    assert_eq!(start(8, play[7] + 1).await, Some(8));
    let high = |at: u64| {
        let store = store.clone();
        async move { store.level_high_water(p, Revision::new(at)).await.unwrap() }
    };
    assert_eq!(high(1).await, None, "nothing readable");
    assert_eq!(high(3).await, Some(10));
    assert_eq!(high(4).await, Some(1), "the reset restarts the mark");
    assert_eq!(high(6).await, Some(9));
    assert_eq!(high(7).await, Some(5), "so does the restore");
    assert_eq!(high(8).await, Some(6));
}

/// Play time grows while a session holds a live lease and stops when it lapses; each revision
/// records the line's play time at its commit.
pub async fn play_time_accrues_only_while_leased<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let a = holder(DeviceId::new());
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    h.advance(60).await;
    let renewed = store.renew(p, a.session, l.token, TTL_MS).await.unwrap();
    let after_renew = store.head(p).await.unwrap().play_ms;
    assert!(after_renew >= 60, "credited on renewal: {after_renew}");
    h.advance(60).await;
    let c = store
        .commit(commit(p, &renewed, Revision::NONE, "timed"))
        .await
        .unwrap();
    assert!(
        c.record.play_ms >= after_renew + 60,
        "{} vs {after_renew}",
        c.record.play_ms
    );
    h.expire_lease(p).await;
    let lapsed = store.head(p).await.unwrap().play_ms;
    h.advance(60).await;
    assert_eq!(
        store.head(p).await.unwrap().play_ms,
        lapsed,
        "a lapsed lease earns nothing"
    );
    let b = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    assert!(
        store.head(p).await.unwrap().play_ms >= lapsed,
        "a new grant starts from what was credited"
    );
    store.release(p, b.holder.session, b.token).await.unwrap();
    let released = store.head(p).await.unwrap().play_ms;
    h.advance(60).await;
    assert_eq!(
        store.head(p).await.unwrap().play_ms,
        released,
        "a free line earns nothing"
    );
}

/// The service answers a retry whose head moved from the key alone: `find_commit`.
pub async fn replays_are_found_by_their_key<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let l = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    let req = commit(p, &l, Revision::NONE, "keyed");
    assert_eq!(store.find_commit(req.commit_id).await.unwrap(), None);
    let done = store.commit(req.clone()).await.unwrap();
    let found = store
        .find_commit(req.commit_id)
        .await
        .unwrap()
        .expect("found");
    assert_eq!(found, done.record);
}

/// A commit or park says whether it created the blob's row, so the caller re-puts the object.
pub async fn writes_report_the_blob_rows_they_create<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile().await;
    let l = grant(&store, p, &holder(DeviceId::new()), AcquireMode::Normal).await;
    let tag = format!("row-{}", CommitId::new().as_uuid());
    let first = store
        .commit(commit(p, &l, Revision::NONE, &tag))
        .await
        .unwrap();
    assert!(first.blob_created);
    let restore = CommitRequest {
        kind: CommitKind::Restore,
        restored_from: Some(Revision::new(1)),
        device: None,
        client_label: None,
        ..commit(p, &l, Revision::new(1), &tag)
    };
    let again = store.commit(restore).await.unwrap();
    assert!(!again.blob_created, "the row existed");
    let replay = store.commit(commit(p, &l, Revision::NONE, &tag)).await;
    assert!(replay.is_err(), "a new key on an old head conflicts");
    let park = |tag: &str| ParkRequest {
        profile: p,
        blob: BlobRef {
            sha256: Digest::of(tag.as_bytes()),
            size: 28_000,
            format: None,
        },
        content: Digest::of(b"content:parked"),
        reason: ParkReason::ConflictLoser,
        based_on: Some(Revision::new(2)),
        session: None,
        device: None,
        summary: None,
        detail: "kept the cloud save".into(),
        retain_ms: 86_400_000,
    };
    let fresh = format!("parked-{}", CommitId::new().as_uuid());
    assert!(store.park(park(&fresh)).await.unwrap().blob_created);
    assert!(!store.park(park(&fresh)).await.unwrap().blob_created);
    let listed = store.parked(p).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed[0].parked_at <= listed[1].parked_at, "oldest first");
}

/// A blob store that holds the first put of one digest until released: a slow Cloud Storage
/// round trip.
#[derive(Clone)]
struct Held {
    inner: MemoryBlobStore,
    hold: Digest,
    armed: Arc<std::sync::atomic::AtomicBool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl BlobStore for Held {
    async fn put(&self, digest: Digest, bytes: &[u8]) -> Result<(), SaveError> {
        if digest == self.hold && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.put(digest, bytes).await
    }

    async fn get(&self, digest: Digest) -> Result<Vec<u8>, SaveError> {
        self.inner.get(digest).await
    }
}

/// The review's reproduction (S1), through the real service: a cheated Hardcore bundle sent with
/// `If-Match` one ahead of the head, racing a legitimate commit that moves the head to exactly
/// that revision while the cheat would sit in its blob put. It must never land, and once the
/// head is its base, the gates refuse it.
pub async fn the_gates_cannot_be_raced<H: Harness>(h: &H) {
    let store = h.store();
    let p = h.new_profile_as(ProfileMode::Hardcore).await;
    let device = DeviceId::new();
    let a = holder(device);
    let l = grant(&store, p, &a, AcquireMode::Normal).await;
    let l = fresh_career(&store, p, &l).await;
    let cheat = save_with_credits(900_000_000, 0x20);
    let blobs = Held {
        inner: MemoryBlobStore::new(),
        hold: Digest::of(&cheat),
        armed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    };
    blobs
        .inner
        .put(Digest::of(starter()), starter())
        .await
        .unwrap();
    let service = Arc::new(SaveService::new(
        store.clone(),
        blobs.clone(),
        SavePolicy::default(),
    ));
    let upload = |expected: u64, bytes: Vec<u8>| UploadRequest {
        profile: p,
        session: a.session,
        token: l.token,
        expected: Revision::new(expected),
        commit_id: CommitId::new(),
        kind: CommitKind::Upload,
        device,
        declared_sha256: Some(Digest::of(&bytes)),
        bytes,
        game_sync: None,
        fresh_start: None,
        seed_from_profile: None,
    };
    let mut attack = {
        let service = service.clone();
        let req = upload(2, cheat.clone());
        tokio::spawn(async move { service.upload(req).await })
    };
    // Whichever comes first: the attack finishing (the fixed service refuses it before any put)
    // or the attack parked in its put (the old service skipped the gates here, then committed).
    let entered = blobs.entered.clone();
    let early = tokio::select! {
        biased;
        done = &mut attack => Some(done.expect("task")),
        () = entered.notified() => None,
    };
    let parked_in_put = early.is_none();
    let legit = service
        .upload(upload(1, save_with_credits(60_000, 0x10)))
        .await
        .expect("the legitimate checkpoint lands");
    assert_eq!(legit.record.revision, Revision::new(2));
    let outcome = match early {
        Some(done) => done,
        None => {
            blobs.release.notify_one();
            attack.await.expect("task")
        }
    };
    assert_eq!(
        outcome
            .as_ref()
            .map(|c| c.record.revision)
            .map_err(SaveError::code),
        Err("SAVES.REVISION_CONFLICT"),
        "an If-Match ahead of the head never commits: {outcome:?}"
    );
    assert!(!parked_in_put, "the attack never reached its blob put");
    // The race window is over: later puts of the same bytes (parking the refused retry) pass.
    blobs
        .armed
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let head = store.head(p).await.unwrap();
    assert_eq!(head.revision, Revision::new(2));
    assert_eq!(head.current.unwrap().summary.credits, Some(60_000));
    // Retried on the head it now names, the cheat is measured and refused (a one-step ceiling,
    // owner 2026-10-27, CLOUD_SAVES.md §7.5).
    let retried = service.upload(upload(2, cheat)).await.unwrap_err();
    assert!(
        matches!(
            retried,
            SaveError::GateRefused {
                rule: "CREDITS_CEILING",
                ..
            }
        ),
        "{retried:?}"
    );
    // And a player restore cannot be used to skip the gates: a restore copies a revision of the
    // line's own history, never new bytes.
    let restored = service
        .restore(RestoreRequest {
            profile: p,
            session: a.session,
            token: l.token,
            expected: Revision::new(2),
            commit_id: CommitId::new(),
            from: Revision::new(1),
            actor: Actor::Player,
        })
        .await
        .unwrap();
    assert_eq!(restored.record.summary.credits, Some(25_000));
}
