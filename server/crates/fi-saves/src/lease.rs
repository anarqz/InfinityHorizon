//! Lease and commit decisions (CLOUD_SAVES.md §4, §5).
//!
//! These functions are pure: given what the store read **under its lock** (the Postgres row lock
//! of `save_head`, or the in-memory mutex) and the store's `now`, they decide. Both stores call the
//! same functions, so the rules cannot drift between the one tests exercise most and the one
//! production runs; the stores only provide atomicity.
//!
//! The safety argument:
//! 1. A grant always issues `fence_seq + 1` and stores it as the line's only lease, so tokens are
//!    unique and strictly increasing per line.
//! 2. A commit is accepted only when the line's current lease has the caller's session and token,
//!    has not lapsed at the store's `now`, and the line's head equals the caller's expected
//!    revision. Steps 1-2 run in one critical section with the write.
//! 3. Therefore at most one session can write at a time (no double writer), and a write never
//!    lands on a head it did not see (no lost update).
//! 4. A player upload on a Hardcore line also names the head its gate verdict was computed on
//!    (`gated_base`), and lands only on exactly that head: no code path can commit a Hardcore
//!    upload whose progress was measured against another revision, or not measured at all.
//!
//! Play time ([`play_credit`]): every write path credits the line with the time since the stored
//! lease was last accounted, if that lease still held at `now`. A lapsed lease earns nothing past
//! its last renewal, so an idle or asleep PC accrues no allowance.

use crate::error::SaveError;
use crate::model::{
    AcquireMode, AcquireRequest, Actor, CommitKind, CommitRequest, FenceToken, Lease, LeaseGrant,
    Released, Replaced, Revision,
};
use fi_domain::{PlaySessionId, ProfileMode, UnixMs};

/// Shortest lease a client may ask for (tests use it; clients use the default).
pub const MIN_TTL_MS: u32 = 1_000;
/// Longest lease: an hour. A crashed PC blocks its profile on other PCs at most this long, and
/// only when the player does not confirm a takeover.
pub const MAX_TTL_MS: u32 = 3_600_000;
/// Default lease: 5 minutes, renewed by the launcher's heartbeat every minute
/// ([`DEFAULT_HEARTBEAT_MS`]) and by every successful commit.
pub const DEFAULT_TTL_MS: u32 = 300_000;
/// Default heartbeat interval: a fifth of the TTL, so four heartbeats can be lost in a row.
pub const DEFAULT_HEARTBEAT_MS: u32 = 60_000;

/// What a store read about a line, under its lock.
#[derive(Clone, Copy, Debug)]
pub struct LineState<'a> {
    /// The profile's mode (Postgres reads `profile.mode` under the same lock).
    pub mode: ProfileMode,
    /// Current head.
    pub revision: Revision,
    /// Highest token ever granted.
    pub fence_seq: u64,
    /// The newest reset revision ([`Revision::NONE`] if never reset).
    pub reset_revision: Revision,
    /// The stored lease (possibly lapsed).
    pub lease: Option<&'a Lease>,
}

fn check_ttl(ttl_ms: u32) -> Result<(), SaveError> {
    if (MIN_TTL_MS..=MAX_TTL_MS).contains(&ttl_ms) {
        Ok(())
    } else {
        Err(SaveError::InvalidArgument(
            "lease ttl must be between 1 s and 1 h",
        ))
    }
}

/// Play time to credit to the line when the stored `lease` is renewed, used by a commit, replaced
/// or released at `now`: the time since it was last accounted, if it still holds at `now`;
/// nothing for a lapsed lease (its holder stopped heartbeating at least a TTL ago).
pub fn play_credit(lease: Option<&Lease>, now: UnixMs) -> u64 {
    match lease {
        Some(l) if l.is_live(now) => now.since(l.accounted_at).max(0) as u64,
        _ => 0,
    }
}

/// The line's play time as seen at `now`: what was credited, plus the live lease's share not
/// credited yet. What [`crate::model::HeadView::play_ms`] reports.
pub fn play_ms_at(credited: u64, lease: Option<&Lease>, now: UnixMs) -> u64 {
    credited.saturating_add(play_credit(lease, now))
}

fn grant(
    state: LineState<'_>,
    req: &AcquireRequest,
    now: UnixMs,
    replaced: Option<Replaced>,
) -> LeaseGrant {
    let token = FenceToken::new(state.fence_seq + 1).unwrap_or(FenceToken::FIRST);
    LeaseGrant {
        lease: Lease {
            holder: req.holder.clone(),
            token,
            acquired_at: now,
            expires_at: now.plus_millis(i64::from(req.ttl_ms)),
            ttl_ms: req.ttl_ms,
            accounted_at: now,
        },
        existing: false,
        replaced,
    }
}

/// Decides an acquire. On `Ok`, unless `grant.existing`, the store credits
/// [`play_credit`]`(state.lease, now)`, writes `grant.lease` as the line's lease and
/// `grant.lease.token` as its `fence_seq`.
pub fn decide_acquire(
    state: LineState<'_>,
    req: &AcquireRequest,
    now: UnixMs,
) -> Result<LeaseGrant, SaveError> {
    check_ttl(req.ttl_ms)?;
    if let AcquireMode::Resume { previous } = req.mode {
        return decide_resume(state, req, previous, now);
    }
    let replaced = match state.lease {
        None => None,
        Some(current) if current.holder.session == req.holder.session && current.is_live(now) => {
            // A retried acquire of the same session: idempotent, nothing changes.
            return Ok(LeaseGrant {
                lease: current.clone(),
                existing: true,
                replaced: None,
            });
        }
        Some(current) if !current.is_live(now) => Some(Replaced::Expired(current.clone())),
        Some(current) if current.holder.device == req.holder.device => {
            Some(Replaced::SameDevice(current.clone()))
        }
        Some(current) => match req.mode {
            AcquireMode::Takeover => Some(Replaced::TakenOver(current.clone())),
            AcquireMode::Normal | AcquireMode::Resume { .. } => {
                return Err(SaveError::LeaseHeld {
                    device: current.holder.device,
                    device_label: current.holder.device_label.clone(),
                    expires_at: current.expires_at,
                });
            }
        },
    };
    Ok(grant(state, req, now, replaced))
}

/// A heartbeat after a lapse: the stored lease must still be this session's `previous` token on
/// this device, and `previous` the newest grant of the line. Then nobody else held the line since
/// (no grant, so no write), and the session's sync mark is still the truth: it gets a new token.
/// A live lease is returned as is (a retry). Anything else (taken over, granted elsewhere after
/// the lapse, or released when the session ended) is `LEASE_LOST`: start a new session.
fn decide_resume(
    state: LineState<'_>,
    req: &AcquireRequest,
    previous: FenceToken,
    now: UnixMs,
) -> Result<LeaseGrant, SaveError> {
    match state.lease {
        Some(current)
            if current.holder.session == req.holder.session
                && current.holder.device == req.holder.device
                && current.token == previous
                && state.fence_seq == previous.get() =>
        {
            if current.is_live(now) {
                Ok(LeaseGrant {
                    lease: current.clone(),
                    existing: true,
                    replaced: None,
                })
            } else {
                Ok(grant(
                    state,
                    req,
                    now,
                    Some(Replaced::Expired(current.clone())),
                ))
            }
        }
        other => Err(SaveError::LeaseLost {
            current: other.map(|l| l.token),
        }),
    }
}

/// The lease of `session` with `token`, or why the caller does not hold it.
fn held<'a>(
    state: LineState<'a>,
    session: PlaySessionId,
    token: FenceToken,
) -> Result<&'a Lease, SaveError> {
    match state.lease {
        Some(lease) if lease.holder.session == session && lease.token == token => Ok(lease),
        other => Err(SaveError::LeaseLost {
            current: other.map(|l| l.token),
        }),
    }
}

/// Whether `session` with `token` may write the line at `now`: it holds the stored lease and the
/// lease has not lapsed. The first two checks of [`check_commit`], usable on an unlocked head
/// read as a cheap precheck (the commit repeats them under the lock).
pub fn check_writer(
    state: LineState<'_>,
    session: PlaySessionId,
    token: FenceToken,
    now: UnixMs,
) -> Result<&Lease, SaveError> {
    let lease = held(state, session, token)?;
    if !lease.is_live(now) {
        return Err(SaveError::LeaseExpired {
            expired_at: lease.expires_at,
        });
    }
    Ok(lease)
}

/// Decides a heartbeat: the renewed lease, to be stored. The store credits
/// [`play_credit`]`(state.lease, now)` first.
pub fn decide_renew(
    state: LineState<'_>,
    session: PlaySessionId,
    token: FenceToken,
    ttl_ms: u32,
    now: UnixMs,
) -> Result<Lease, SaveError> {
    check_ttl(ttl_ms)?;
    let lease = check_writer(state, session, token, now)?;
    Ok(Lease {
        expires_at: now.plus_millis(i64::from(ttl_ms)),
        ttl_ms,
        accounted_at: now,
        ..lease.clone()
    })
}

/// Decides a release. `Released` means the store credits [`play_credit`]`(state.lease, now)`
/// and clears the lease; a lapsed lease of the same session and token may still be released (end
/// of a session whose heartbeat was late).
pub fn decide_release(
    state: LineState<'_>,
    session: PlaySessionId,
    token: FenceToken,
) -> Result<Released, SaveError> {
    match state.lease {
        None => Ok(Released::AlreadyReleased),
        Some(_) => held(state, session, token).map(|_| Released::Released),
    }
}

/// The rules of what a commit may be, beyond who writes on which head: kinds and their actors,
/// restores, accepted parked bundles, how each mode's line starts, and the Hardcore gate binding.
fn check_kind(state: LineState<'_>, req: &CommitRequest) -> Result<(), SaveError> {
    if !req.kind.allows(req.actor) {
        return Err(SaveError::InvalidArgument(
            "that actor cannot write that kind of revision",
        ));
    }
    match (req.kind, req.restored_from) {
        (CommitKind::Restore, Some(from)) if !from.is_none() && from <= req.expected => {
            // A player may not undo a reset by bringing back what it replaced; a moderator may.
            if req.actor == Actor::Player && from < state.reset_revision {
                return Err(SaveError::RestoreBehindReset {
                    reset: state.reset_revision,
                });
            }
        }
        (CommitKind::Restore, _) => {
            return Err(SaveError::InvalidArgument(
                "a restore names an earlier revision",
            ));
        }
        (_, Some(_)) => {
            return Err(SaveError::InvalidArgument(
                "only a restore names a source revision",
            ));
        }
        (_, None) => {}
    }
    if (req.kind == CommitKind::ParkedAccept) != req.parked_from.is_some() {
        return Err(SaveError::InvalidArgument(
            "an accepted parked bundle, and only that, names its parked id",
        ));
    }
    let gated = state.mode.is_gated();
    match req.kind {
        CommitKind::FreshCareer if !gated || !state.revision.is_none() => Err(
            SaveError::InvalidArgument("a fresh career is a hardcore line's first revision"),
        ),
        CommitKind::FreshStart if !gated || !state.revision.is_none() => Err(
            SaveError::InvalidArgument("a fresh start is a hardcore line's first revision"),
        ),
        CommitKind::FreshStart if req.gated_base != Some(state.revision) => {
            Err(SaveError::InvalidArgument(
                "a fresh start lands only on the empty line its check measured",
            ))
        }
        CommitKind::SeedFromProfile if !gated || !state.revision.is_none() => Err(
            SaveError::InvalidArgument("a seed from a profile is a hardcore line's first revision"),
        ),
        CommitKind::SeedFromProfile if req.gated_base != Some(state.revision) => {
            Err(SaveError::InvalidArgument(
                "a seed from a profile lands only on the empty line its check measured",
            ))
        }
        CommitKind::SeedFromProfile if req.seed_source.is_none() => Err(
            SaveError::InvalidArgument("a seed from a profile names its source profile"),
        ),
        _ if req.kind != CommitKind::SeedFromProfile && req.seed_source.is_some() => Err(
            SaveError::InvalidArgument("only a seed from a profile names a source profile"),
        ),
        CommitKind::ImportOffline if gated => Err(SaveError::HardcoreNeedsFreshCareer),
        CommitKind::ImportOffline if !state.revision.is_none() => Err(SaveError::InvalidArgument(
            "an offline import starts an empty noob line",
        )),
        CommitKind::Upload | CommitKind::ResolveKeepLocal if gated => {
            if state.revision.is_none() {
                Err(SaveError::HardcoreNeedsFreshCareer)
            } else if req.gated_base != Some(state.revision) {
                Err(SaveError::InvalidArgument(
                    "a hardcore upload lands only on the head its gates measured",
                ))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// Checks a commit against the line; on `Ok` the store credits [`play_credit`]`(state.lease,
/// now)`, writes revision `expected + 1` and stores the returned lease (its expiry extended: any
/// successful commit counts as a heartbeat).
pub fn check_commit(
    state: LineState<'_>,
    req: &CommitRequest,
    now: UnixMs,
) -> Result<Lease, SaveError> {
    let lease = check_writer(state, req.session, req.token, now)?;
    if state.revision != req.expected {
        return Err(SaveError::RevisionConflict {
            expected: req.expected,
            head: state.revision,
        });
    }
    check_kind(state, req)?;
    if let Some(label) = &req.client_label
        && (label.len() != 16
            || !label
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)))
    {
        return Err(SaveError::InvalidArgument(
            "client label is 16 upper-case hex digits",
        ));
    }
    let extended = now
        .plus_millis(i64::from(lease.ttl_ms))
        .max(lease.expires_at);
    Ok(Lease {
        expires_at: extended,
        accounted_at: now,
        ..lease.clone()
    })
}

/// The reset revision after a commit of `kind` lands as `revision` (stores write it with the
/// revision).
pub fn reset_after(kind: CommitKind, revision: Revision, previous: Revision) -> Revision {
    if kind.is_reset() { revision } else { previous }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, BlobRef, CommitId, Digest, LeaseHolder, SaveSummary};
    use fi_domain::{DeviceId, ProfileId};

    const T0: UnixMs = UnixMs::from_millis(1_000_000);

    fn holder(device: DeviceId) -> LeaseHolder {
        LeaseHolder {
            session: PlaySessionId::new(),
            device,
            device_label: "PC".into(),
        }
    }

    fn acquire(holder: &LeaseHolder, mode: AcquireMode) -> AcquireRequest {
        AcquireRequest {
            holder: holder.clone(),
            ttl_ms: 10_000,
            mode,
        }
    }

    fn state(revision: u64, fence_seq: u64, lease: Option<&Lease>) -> LineState<'_> {
        LineState {
            mode: ProfileMode::Noob,
            revision: Revision::new(revision),
            fence_seq,
            reset_revision: Revision::NONE,
            lease,
        }
    }

    fn hardcore(state: LineState<'_>) -> LineState<'_> {
        LineState {
            mode: ProfileMode::Hardcore,
            ..state
        }
    }

    fn commit(lease: &Lease, expected: u64) -> CommitRequest {
        CommitRequest {
            profile: ProfileId::new(),
            session: lease.holder.session,
            token: lease.token,
            expected: Revision::new(expected),
            commit_id: CommitId::new(),
            blob: BlobRef {
                sha256: Digest::of(b"x"),
                size: 1,
                format: None,
            },
            content: Digest::of(b"c"),
            kind: CommitKind::Upload,
            actor: Actor::Player,
            device: Some(lease.holder.device),
            client_label: Some("00000000000000A1".into()),
            restored_from: None,
            summary: SaveSummary::default(),
            gated_base: None,
            parked_from: None,
            guard_input: None,
            seed_source: None,
        }
    }

    #[test]
    fn a_free_line_grants_the_next_token() {
        let a = holder(DeviceId::new());
        let g = decide_acquire(state(0, 41, None), &acquire(&a, AcquireMode::Normal), T0).unwrap();
        assert_eq!(g.lease.token.get(), 42);
        assert_eq!(g.lease.expires_at, T0.plus_millis(10_000));
        assert_eq!(g.lease.accounted_at, T0);
        assert!(!g.existing);
        assert_eq!(g.replaced, None);
    }

    #[test]
    fn a_live_lease_of_another_device_is_held_unless_taken_over() {
        let a = holder(DeviceId::new());
        let b = holder(DeviceId::new());
        let lease = decide_acquire(state(0, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let err = decide_acquire(
            state(0, 1, Some(&lease)),
            &acquire(&b, AcquireMode::Normal),
            T0,
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::LeaseHeld { device, .. } if device == a.device));
        let g = decide_acquire(
            state(0, 1, Some(&lease)),
            &acquire(&b, AcquireMode::Takeover),
            T0,
        )
        .unwrap();
        assert_eq!(g.lease.token.get(), 2);
        assert!(matches!(g.replaced, Some(Replaced::TakenOver(_))));
    }

    #[test]
    fn retries_of_the_same_session_change_nothing() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(0, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let again = decide_acquire(
            state(0, 1, Some(&lease)),
            &acquire(&a, AcquireMode::Normal),
            T0.plus_millis(5),
        )
        .unwrap();
        assert!(again.existing);
        assert_eq!(again.lease, lease);
    }

    #[test]
    fn a_new_session_on_the_same_device_replaces_the_crashed_one() {
        let device = DeviceId::new();
        let old = holder(device);
        let new = holder(device);
        let lease = decide_acquire(state(0, 0, None), &acquire(&old, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let g = decide_acquire(
            state(0, 1, Some(&lease)),
            &acquire(&new, AcquireMode::Normal),
            T0,
        )
        .unwrap();
        assert!(matches!(g.replaced, Some(Replaced::SameDevice(_))));
        assert_eq!(g.lease.token.get(), 2);
    }

    #[test]
    fn an_expired_lease_is_replaced_and_fenced() {
        let a = holder(DeviceId::new());
        let b = holder(DeviceId::new());
        let lease = decide_acquire(state(3, 7, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let later = lease.expires_at;
        let g = decide_acquire(
            state(3, 8, Some(&lease)),
            &acquire(&b, AcquireMode::Normal),
            later,
        )
        .unwrap();
        assert!(matches!(g.replaced, Some(Replaced::Expired(_))));
        // The old holder's commit, even with the right expected revision, is fenced.
        let err = check_commit(state(3, 9, Some(&g.lease)), &commit(&lease, 3), later).unwrap_err();
        assert_eq!(
            err,
            SaveError::LeaseLost {
                current: Some(g.lease.token)
            }
        );
    }

    #[test]
    fn resume_regrants_only_a_lapsed_lease_nobody_took() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(4, 6, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        assert_eq!(lease.token.get(), 7);
        let resume = |previous: FenceToken| acquire(&a, AcquireMode::Resume { previous });
        // Still live: a retry, nothing changes.
        let live = decide_acquire(state(4, 7, Some(&lease)), &resume(lease.token), T0).unwrap();
        assert!(live.existing);
        // Lapsed, nobody took it: a new token for the same session.
        let late = lease.expires_at.plus_millis(60_000);
        let back = decide_acquire(state(4, 7, Some(&lease)), &resume(lease.token), late).unwrap();
        assert_eq!(back.lease.token.get(), 8);
        assert!(matches!(back.replaced, Some(Replaced::Expired(_))));
        assert_eq!(back.lease.holder.session, a.session);
        // Someone was granted the line after the lapse: lost, whatever the stored state says now.
        let b = holder(DeviceId::new());
        let taken = decide_acquire(
            state(4, 7, Some(&lease)),
            &acquire(&b, AcquireMode::Normal),
            late,
        )
        .unwrap()
        .lease;
        assert_eq!(
            decide_acquire(state(4, 8, Some(&taken)), &resume(lease.token), late).unwrap_err(),
            SaveError::LeaseLost {
                current: Some(taken.token)
            }
        );
        // Released (the session ended): lost; the launcher starts a new session.
        assert_eq!(
            decide_acquire(state(4, 7, None), &resume(lease.token), late).unwrap_err(),
            SaveError::LeaseLost { current: None }
        );
        // Another device presenting the same session id and token: lost.
        let mut stranger = resume(lease.token);
        stranger.holder.device = DeviceId::new();
        assert!(matches!(
            decide_acquire(state(4, 7, Some(&lease)), &stranger, late),
            Err(SaveError::LeaseLost { .. })
        ));
        // A wrong previous token: lost.
        assert!(matches!(
            decide_acquire(
                state(4, 7, Some(&lease)),
                &resume(FenceToken::new(6).unwrap()),
                late
            ),
            Err(SaveError::LeaseLost { .. })
        ));
    }

    #[test]
    fn commits_need_a_live_lease_and_the_current_head() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(5, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let ok = check_commit(
            state(5, 1, Some(&lease)),
            &commit(&lease, 5),
            T0.plus_millis(9_000),
        )
        .unwrap();
        assert_eq!(
            ok.expires_at,
            T0.plus_millis(19_000),
            "a commit renews the lease"
        );
        assert_eq!(ok.accounted_at, T0.plus_millis(9_000));
        assert_eq!(
            check_commit(state(6, 1, Some(&lease)), &commit(&lease, 5), T0).unwrap_err(),
            SaveError::RevisionConflict {
                expected: Revision::new(5),
                head: Revision::new(6)
            }
        );
        assert_eq!(
            check_commit(
                state(5, 1, Some(&lease)),
                &commit(&lease, 5),
                lease.expires_at
            )
            .unwrap_err(),
            SaveError::LeaseExpired {
                expired_at: lease.expires_at
            }
        );
        let mut wrong_token = commit(&lease, 5);
        wrong_token.token = FenceToken::new(9).unwrap();
        assert!(matches!(
            check_commit(state(5, 1, Some(&lease)), &wrong_token, T0),
            Err(SaveError::LeaseLost { .. })
        ));
        assert!(matches!(
            check_commit(state(5, 1, None), &commit(&lease, 5), T0),
            Err(SaveError::LeaseLost { current: None })
        ));
    }

    #[test]
    fn hardcore_uploads_land_only_on_the_head_they_were_gated_against() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(3, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let line = hardcore(state(3, 1, Some(&lease)));
        let ungated = commit(&lease, 3);
        assert!(
            matches!(
                check_commit(line, &ungated, T0),
                Err(SaveError::InvalidArgument(_))
            ),
            "an ungated hardcore upload never lands, even on the right head"
        );
        let mut stale_verdict = commit(&lease, 3);
        stale_verdict.gated_base = Some(Revision::new(2));
        assert!(matches!(
            check_commit(line, &stale_verdict, T0),
            Err(SaveError::InvalidArgument(_))
        ));
        let mut gated = commit(&lease, 3);
        gated.gated_base = Some(Revision::new(3));
        assert!(check_commit(line, &gated, T0).is_ok());
        let mut keep_local = gated.clone();
        keep_local.kind = CommitKind::ResolveKeepLocal;
        keep_local.gated_base = None;
        assert!(
            check_commit(line, &keep_local, T0).is_err(),
            "keep-local is gated too"
        );
        assert!(
            check_commit(state(3, 1, Some(&lease)), &ungated, T0).is_ok(),
            "noob uploads are not gated"
        );
        let empty = hardcore(state(0, 1, Some(&lease)));
        assert_eq!(
            check_commit(empty, &commit(&lease, 0), T0).unwrap_err(),
            SaveError::HardcoreNeedsFreshCareer
        );
    }

    #[test]
    fn lines_start_by_their_mode() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(0, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let mut import = commit(&lease, 0);
        import.kind = CommitKind::ImportOffline;
        assert!(check_commit(state(0, 1, Some(&lease)), &import, T0).is_ok());
        assert_eq!(
            check_commit(hardcore(state(0, 1, Some(&lease))), &import, T0).unwrap_err(),
            SaveError::HardcoreNeedsFreshCareer,
            "an offline career never starts a hardcore line"
        );
        let mut later_import = commit(&lease, 4);
        later_import.kind = CommitKind::ImportOffline;
        assert!(
            check_commit(state(4, 1, Some(&lease)), &later_import, T0).is_err(),
            "an offline import only starts an empty line"
        );
        let mut fresh = commit(&lease, 0);
        fresh.kind = CommitKind::FreshCareer;
        fresh.actor = Actor::System;
        fresh.client_label = None;
        assert!(check_commit(hardcore(state(0, 1, Some(&lease))), &fresh, T0).is_ok());
        assert!(check_commit(state(0, 1, Some(&lease)), &fresh, T0).is_err());
        let mut late_fresh = fresh.clone();
        late_fresh.expected = Revision::new(2);
        assert!(check_commit(hardcore(state(2, 1, Some(&lease))), &late_fresh, T0).is_err());
        let mut forged = fresh;
        forged.actor = Actor::Player;
        assert!(
            check_commit(hardcore(state(0, 1, Some(&lease))), &forged, T0).is_err(),
            "kinds have fixed writers"
        );
    }

    #[test]
    fn restores_must_name_an_earlier_revision_after_the_last_reset() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(4, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let mut req = commit(&lease, 4);
        req.kind = CommitKind::Restore;
        assert!(
            check_commit(state(4, 1, Some(&lease)), &req, T0).is_err(),
            "no source"
        );
        req.restored_from = Some(Revision::new(5));
        assert!(
            check_commit(state(4, 1, Some(&lease)), &req, T0).is_err(),
            "a later source"
        );
        req.restored_from = Some(Revision::new(2));
        assert!(check_commit(state(4, 1, Some(&lease)), &req, T0).is_ok());
        let mut upload = commit(&lease, 4);
        upload.restored_from = Some(Revision::new(2));
        assert!(
            check_commit(state(4, 1, Some(&lease)), &upload, T0).is_err(),
            "only restores name a source"
        );
        let reset_at_3 = LineState {
            reset_revision: Revision::new(3),
            ..state(4, 1, Some(&lease))
        };
        assert_eq!(
            check_commit(reset_at_3, &req, T0).unwrap_err(),
            SaveError::RestoreBehindReset {
                reset: Revision::new(3)
            },
            "a player cannot bring back what a reset replaced"
        );
        let mut moderator = req.clone();
        moderator.actor = Actor::Moderator;
        assert!(
            check_commit(reset_at_3, &moderator, T0).is_ok(),
            "a moderator can"
        );
        req.restored_from = Some(Revision::new(3));
        assert!(
            check_commit(reset_at_3, &req, T0).is_ok(),
            "the reset itself is fine"
        );
    }

    #[test]
    fn accepted_parked_bundles_are_moderator_commits_naming_their_bundle() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(2, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let mut accept = commit(&lease, 2);
        accept.kind = CommitKind::ParkedAccept;
        accept.actor = Actor::Moderator;
        accept.client_label = None;
        let line = hardcore(state(2, 1, Some(&lease)));
        assert!(
            check_commit(line, &accept, T0).is_err(),
            "names its parked bundle"
        );
        accept.parked_from = Some(uuid::Uuid::now_v7());
        assert!(
            check_commit(line, &accept, T0).is_ok(),
            "not gated: a moderator decided"
        );
        let mut stray = commit(&lease, 2);
        stray.parked_from = accept.parked_from;
        assert!(check_commit(state(2, 1, Some(&lease)), &stray, T0).is_err());
    }

    #[test]
    fn renew_and_release() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(0, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        let renewed = decide_renew(
            state(0, 1, Some(&lease)),
            a.session,
            lease.token,
            20_000,
            T0.plus_millis(1),
        )
        .unwrap();
        assert_eq!(renewed.expires_at, T0.plus_millis(20_001));
        assert_eq!(renewed.accounted_at, T0.plus_millis(1));
        assert_eq!(
            decide_renew(
                state(0, 1, Some(&lease)),
                a.session,
                lease.token,
                20_000,
                lease.expires_at
            )
            .unwrap_err(),
            SaveError::LeaseExpired {
                expired_at: lease.expires_at
            }
        );
        assert!(
            decide_renew(state(0, 1, Some(&lease)), a.session, lease.token, 999, T0).is_err(),
            "ttl bounds"
        );
        assert_eq!(
            decide_release(state(0, 1, Some(&lease)), a.session, lease.token),
            Ok(Released::Released)
        );
        assert_eq!(
            decide_release(state(0, 1, None), a.session, lease.token),
            Ok(Released::AlreadyReleased)
        );
        let other = PlaySessionId::new();
        assert!(matches!(
            decide_release(state(0, 1, Some(&lease)), other, lease.token),
            Err(SaveError::LeaseLost { .. })
        ));
    }

    #[test]
    fn play_time_accrues_only_while_the_lease_holds() {
        let a = holder(DeviceId::new());
        let lease = decide_acquire(state(0, 0, None), &acquire(&a, AcquireMode::Normal), T0)
            .unwrap()
            .lease;
        assert_eq!(play_credit(Some(&lease), T0.plus_millis(4_000)), 4_000);
        assert_eq!(play_ms_at(100, Some(&lease), T0.plus_millis(4_000)), 4_100);
        assert_eq!(
            play_credit(Some(&lease), lease.expires_at),
            0,
            "a lapsed lease earns nothing past its last renewal"
        );
        assert_eq!(play_credit(None, T0), 0);
        let early = UnixMs::from_millis(T0.millis() - 50);
        assert_eq!(
            play_credit(Some(&lease), early),
            0,
            "a clock step back credits nothing"
        );
        assert_eq!(
            reset_after(CommitKind::CareerReset, Revision::new(9), Revision::new(2)),
            Revision::new(9)
        );
        assert_eq!(
            reset_after(CommitKind::Upload, Revision::new(9), Revision::new(2)),
            Revision::new(2)
        );
    }
}
