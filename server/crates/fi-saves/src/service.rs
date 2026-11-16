//! The save use cases over a [`SaveStore`], a [`BlobStore`] and a [`Verifier`] (CLOUD_SAVES.md
//! §5-§8).
//!
//! Upload order, which makes a crash anywhere safe and keeps the expensive work behind the cheap
//! refusals:
//! 1. size and digest checks;
//! 2. read the head. If it is not `If-Match`, nothing is ever committed: a retry whose commit
//!    already landed (lost response) is answered from its idempotency key, anything else is a
//!    conflict. The caller must hold the live lease;
//! 3. rules that need no bytes: a Hardcore line never starts from an upload, an offline import only
//!    starts an empty Noob line;
//! 4. verify the bundle (CPU-bound: the [`Verifier`] decides where it runs and how many at once);
//! 5. Hardcore: refuse a career a reset replaced and numbers beyond the game's limits, then (the
//!    hybrid Hardcore checks, owner 2026-10-26 and 2026-10-27, CLOUD_SAVES.md §7.5) only the
//!    inline checks against the newest revision with readable numbers ([`gates::inline_checks`]:
//!    the one-step ceilings, a level its XP does not give, tokens the skill points did not earn,
//!    XP or a level below the base; no rate replay); either refuses whatever the enforcement
//!    switch, and a refusal parks the bundle (the same bundle refused again on the same head is not parked
//!    twice, review G2-11). The full guardrails ([`guardrails`]: rates replayed from the line's
//!    history, ceilings, the anomaly score) run in the nightly pass over the revision this
//!    commit records with its [`GuardInput`] ([`nightly_assessment`], `fi-cli integrity-nightly`);
//! 6. put the blob (content-addressed and create-only: a retry is a no-op);
//! 7. commit (lease check, CAS, gate binding and idempotency in one critical section), naming the
//!    head the gates measured (`gated_base`), so the store refuses it on any other head;
//! 8. if the commit created the blob's row, put the object again (a garbage collection that ran
//!    between steps 6 and 7 may have deleted it, CLOUD_SAVES.md §6).
//!
//! Steps 1-5 are one function ([`SaveService::check_in`]) that only reads; the dry run
//! ([`SaveService::check_upload`], CR-S71) runs exactly them and stores nothing.
//!
//! A crash after 6 and before 7 leaves an unreferenced blob that garbage collection removes; the
//! line never points at a missing blob. The service holds no lock across steps: the CAS and the
//! gate binding in step 7 are what make the head read in step 2 the head the commit lands on.

use crate::blob::BlobStore;
use crate::bundle::{self, BundleError, MAX_BUNDLE, Verified};
use crate::error::SaveError;
use crate::fresh::{self, FreshStartCheck};
use crate::gates::{self, GatePolicy};
use crate::lease::{self, LineState};
use crate::model::{
    Actor, BlobRef, CommitId, CommitKind, CommitRequest, Committed, Digest, FenceToken, GuardInput,
    HeadView, ParkReason, ParkRequest, ParkedRecord, Revision, RevisionRecord, SaveSummary,
};
use crate::seed::{self, SeedFromProfileCheck};
use crate::store::SaveStore;
use fi_domain::{DeviceId, PlaySessionId, ProfileId, UnixMs};
use fi_guard::{Judged, Rates};
use std::future::Future;
use std::sync::Arc;
use uuid::Uuid;

/// How many revisions below the head the gates look through for the newest one with readable
/// numbers, the pre-reset check for a career a reset replaced, and the guardrails' replay of the
/// line's rates.
pub const HISTORY_SEARCH: u32 = 100;

/// The `rule` of a refusal of a career a reset replaced (`SAVES.GATE_REFUSED.rule`).
pub const REPLACED_BY_RESET: &str = "REPLACED_BY_RESET";

/// Play time back from the gated base over which one step carries the line's fast rates (6 h: what
/// is older weighs less than e^-6, 0.25%, in a one-hour rate).
pub const RATE_WINDOW_MS: u64 = 6 * 3_600_000;

/// Play time back from the gated base over which the guardrails replay the line's rates and one
/// step carries its slow ones (72 h, three of the slow rate's 24-hour time constants: what is
/// older weighs less than e^-3, 5%; review G2-1).
pub const SLOW_WINDOW_MS: u64 = 72 * 3_600_000;

/// A line's bundle format, by its profile's game (CR-S105): FH2's `FH2SAVE1` ([`bundle`]) or
/// FM5's `FM5SAVE1` ([`crate::fm5`]). A line takes only its own format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SaveFormat {
    /// FH2SAVE1, at most [`bundle::MAX_BUNDLE`] (every game but FM5, as before CR-S105).
    #[default]
    Fh2Save1,
    /// FM5SAVE1, at most [`crate::fm5::MAX_BUNDLE`].
    Fm5Save1,
}

impl SaveFormat {
    /// The format of a line of `game` (a game code): FM5's for `fm5`, FH2's for every other.
    pub fn of_game(game: &str) -> Self {
        if game == fi_domain::FM5 {
            Self::Fm5Save1
        } else {
            Self::Fh2Save1
        }
    }

    /// The largest bundle of the format.
    pub fn max_bundle(self) -> usize {
        match self {
            Self::Fh2Save1 => MAX_BUNDLE,
            Self::Fm5Save1 => crate::fm5::MAX_BUNDLE,
        }
    }

    /// The name `save_blob.format` stores (`fh2save1`, `fm5save1`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Fh2Save1 => "fh2save1",
            Self::Fm5Save1 => "fm5save1",
        }
    }

    /// The format `bytes`' magic names (`FH2SAVE1`, `FM5SAVE1`), whatever line they were sent to;
    /// `None` for neither (CR-S105 review 4: `save_blob.format` follows the bytes).
    pub fn of_magic(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(crate::fm5::MAGIC) {
            Some(Self::Fm5Save1)
        } else if bytes.starts_with(crate::bundle::MAGIC) {
            Some(Self::Fh2Save1)
        } else {
            None
        }
    }

    /// Whether `bytes` may be verified as this format: an FM5 line only an `FM5SAVE1` bundle; an
    /// FH2 line anything but one (FH2's own checks then answer exactly as before CR-S105).
    pub fn admits(self, bytes: &[u8]) -> bool {
        let fm5 = bytes.starts_with(crate::fm5::MAGIC);
        match self {
            Self::Fh2Save1 => !fm5,
            Self::Fm5Save1 => fm5,
        }
    }
}

/// Tunables of the save service.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SavePolicy {
    /// The Hardcore gates: the game's limits, its rules and the guardrails' enforcement switch.
    pub gates: GatePolicy,
    /// How long parked bundles are kept (90 days by default).
    pub park_retention_ms: i64,
    /// The most bundles a profile's player may have parked at once (refused, unreadable, conflict
    /// losers; 20, owner 2026-10-28): past it a new one is refused, not parked, and stays on the
    /// PC. The server's own copies (pre-restore, pre-reset) are not counted.
    pub max_parked: usize,
    /// The line's bundle format (CR-S105): its size limit and which bundles it verifies.
    pub format: SaveFormat,
}

impl Default for SavePolicy {
    fn default() -> Self {
        Self {
            gates: GatePolicy::default(),
            park_retention_ms: 90 * 24 * 3_600_000,
            max_parked: 20,
            format: SaveFormat::Fh2Save1,
        }
    }
}

/// Where bundle verification runs. It is CPU-bound (tens of milliseconds and one 6 MB buffer for
/// a real save), so the API runs it on a blocking thread under a per-instance limit, answering
/// `SAVES.UNAVAILABLE` (retryable, `Retry-After`) when the limit is reached (SAVES_API.md §4.3);
/// tests and tools run it inline with [`InlineVerifier`].
pub trait Verifier: Send + Sync {
    /// [`bundle::verify`] of `bytes`, or `SaveError::Unavailable` when no capacity is left.
    fn verify(&self, bytes: Arc<[u8]>) -> impl Future<Output = Result<Verified, SaveError>> + Send;
}

/// Verifies on the caller's thread.
#[derive(Clone, Copy, Debug, Default)]
pub struct InlineVerifier;

impl Verifier for InlineVerifier {
    fn verify(&self, bytes: Arc<[u8]>) -> impl Future<Output = Result<Verified, SaveError>> + Send {
        // By magic (CR-S105); the service admits only its line's format first.
        let verified = if bytes.starts_with(crate::fm5::MAGIC) {
            crate::fm5::verify(&bytes)
        } else {
            bundle::verify(&bytes)
        };
        std::future::ready(verified.map_err(SaveError::BundleInvalid))
    }
}

/// A player's upload (`PUT .../save`).
#[derive(Clone, Debug)]
pub struct UploadRequest {
    /// Whose line.
    pub profile: ProfileId,
    /// The session holding the lease.
    pub session: PlaySessionId,
    /// Its fencing token (`X-Fi-Fence`).
    pub token: FenceToken,
    /// `If-Match`.
    pub expected: Revision,
    /// `Idempotency-Key`.
    pub commit_id: CommitId,
    /// [`CommitKind::Upload`], [`CommitKind::ResolveKeepLocal`] or [`CommitKind::ImportOffline`].
    pub kind: CommitKind,
    /// The uploading launcher.
    pub device: DeviceId,
    /// The SHA-256 the client declared (`Repr-Digest`), if any.
    pub declared_sha256: Option<Digest>,
    /// The FH2SAVE1 bundle.
    pub bytes: Vec<u8>,
    /// The profile's last accepted game sync (the RPC baseline's numbers and the hours since it,
    /// read by the caller), which a Hardcore upload is compared with; `None`: no comparison.
    pub game_sync: Option<fi_guard::GameSync>,
    /// CR-S51 (SAVES_API.md §4.9): `Some` when the line's start is `fresh_start` (the server's
    /// `FI_HARDCORE_START`, or no active seed), with the check its [`CommitKind::FreshStart`]
    /// commit passes; `None` for a line that starts from the seed. Only read on a Hardcore line at
    /// revision 0: a `fresh_start` there needs it, and every other kind is refused either way.
    pub fresh_start: Option<FreshStartCheck>,
    /// CR-S52 (SAVES_API.md §4.10): `Some` for a [`CommitKind::SeedFromProfile`] commit the server
    /// allows (`FI_HARDCORE_FROM_PROFILE` on and the line's start `fresh_start`), with its source
    /// profile and the play time its seed check measures over; `None` refuses that kind.
    pub seed_from_profile: Option<crate::seed::SeedFromProfileCheck>,
}

/// An upload's result with the assessment of a refusal by the inline checks (the hybrid Hardcore
/// checks, CLOUD_SAVES.md §7.5: [`gates::inline_checks`]), which the caller records. An accepted upload carries none: the nightly pass
/// assesses its revision ([`nightly_assessment`]). The same bytes refused again on the same head
/// carry none either (recorded once, review G2-11).
#[derive(Debug)]
pub struct UploadOutcome {
    /// What [`SaveService::upload`] answers.
    pub result: Result<Committed, SaveError>,
    /// The inline checks' assessment that refused the upload.
    pub assessment: Option<fi_guard::Assessment>,
}

/// The bundle's verdict of an upload dry run ([`SaveService::check_upload`], CR-S71,
/// SAVES_API.md §4.11): what [`SaveService::upload`] would decide on these bytes now.
#[derive(Clone, Debug, PartialEq)]
pub enum UploadVerdict {
    /// The upload would be committed, with these numbers.
    Pass {
        /// The numbers the revision would record.
        summary: SaveSummary,
    },
    /// The upload would be refused after verification.
    Refused {
        /// The error code the upload would answer: `SAVES.GATE_REFUSED`,
        /// `SAVES.SUMMARY_UNREADABLE` or `SAVES.BUNDLE_INVALID`.
        code: &'static str,
        /// `SAVES.GATE_REFUSED`'s rule; `None` for the other codes.
        rule: Option<&'static str>,
        /// The refusal's message, as the upload's (in English, no player data).
        reason: String,
        /// The fact checks' score when they decided it (as the upload's refusal).
        score: Option<u8>,
        /// The assessment's reason codes, or the check's (`FRESH_*`, the seed check's).
        reasons: Vec<&'static str>,
    },
}

impl UploadVerdict {
    /// Whether the upload would be committed.
    pub fn passes(&self) -> bool {
        matches!(self, UploadVerdict::Pass { .. })
    }
}

/// What steps 1-5 of an upload decided ([`SaveService::check_in`]).
enum Verdict {
    /// The head moved and the upload's key already committed these bytes: the replay.
    Replayed(RevisionRecord),
    /// Steps 6-8 may run: commit with these.
    Pass {
        summary: SaveSummary,
        gated_base: Option<Revision>,
        guard_input: Option<GuardInput>,
        /// A `seed_from_profile` commit's check (its own commit, naming the source).
        seed: Option<SeedFromProfileCheck>,
    },
    /// A Hardcore save whose numbers do not read; `again`: these bytes already parked so.
    Unreadable { again: Option<Uuid> },
    /// A gate refused it; `again`: these bytes already parked for it on this head.
    Refused {
        again: Option<Uuid>,
        reason: String,
        rule: &'static str,
        score: Option<u8>,
        reasons: Vec<&'static str>,
    },
}

/// The checked upload: its bytes and what the checks decided.
struct Checked {
    blob: BlobRef,
    bytes: Arc<[u8]>,
    /// `None` only for [`Verdict::Replayed`].
    verified: Option<Verified>,
    /// The assessment the decision records (none when these bytes were already refused here).
    assessment: Option<fi_guard::Assessment>,
    verdict: Verdict,
}

/// A restore (`POST .../save/restore`): an older revision committed as the newest.
#[derive(Clone, Debug)]
pub struct RestoreRequest {
    /// Whose line.
    pub profile: ProfileId,
    /// The session holding the lease.
    pub session: PlaySessionId,
    /// Its token.
    pub token: FenceToken,
    /// The head the player saw when choosing.
    pub expected: Revision,
    /// Idempotency key.
    pub commit_id: CommitId,
    /// The revision to bring back.
    pub from: Revision,
    /// Player (VIP self-service) or moderator (support). Players cannot reach behind a reset.
    pub actor: Actor,
}

/// A moderation, season or career reset, under a lease the caller took over from the player.
#[derive(Clone, Debug)]
pub struct ResetRequest {
    /// Whose line.
    pub profile: ProfileId,
    /// The system session holding the lease.
    pub session: PlaySessionId,
    /// Its token.
    pub token: FenceToken,
    /// The head being reset.
    pub expected: Revision,
    /// Idempotency key.
    pub commit_id: CommitId,
    /// [`CommitKind::ModerationReset`], [`CommitKind::SeasonReset`] or [`CommitKind::CareerReset`].
    pub kind: CommitKind,
    /// [`Actor::Moderator`] or [`Actor::System`].
    pub actor: Actor,
    /// The reset career (the server's SQLite editor, SER-M2).
    pub bytes: Vec<u8>,
}

/// A moderator accepting a parked bundle after review (a gate refusal overturned).
#[derive(Clone, Debug)]
pub struct AcceptParkedRequest {
    /// Whose line.
    pub profile: ProfileId,
    /// The moderator's session holding the lease.
    pub session: PlaySessionId,
    /// Its token.
    pub token: FenceToken,
    /// The head the moderator saw.
    pub expected: Revision,
    /// Idempotency key.
    pub commit_id: CommitId,
    /// The parked bundle.
    pub parked: Uuid,
}

/// Save use cases.
pub struct SaveService<S, B, V = InlineVerifier> {
    store: S,
    blobs: B,
    verifier: V,
    policy: SavePolicy,
}

impl<S: SaveStore, B: BlobStore> SaveService<S, B, InlineVerifier> {
    /// A service over `store` and `blobs` that verifies bundles inline.
    pub fn new(store: S, blobs: B, policy: SavePolicy) -> Self {
        Self::with_verifier(store, blobs, InlineVerifier, policy)
    }
}

fn line_state(head: &HeadView) -> LineState<'_> {
    LineState {
        mode: head.mode,
        revision: head.revision,
        fence_seq: head.fence_seq,
        reset_revision: head.reset_revision,
        lease: head.lease.as_ref(),
    }
}

impl<S: SaveStore, B: BlobStore, V: Verifier> SaveService<S, B, V> {
    /// A service with its own [`Verifier`].
    pub fn with_verifier(store: S, blobs: B, verifier: V, policy: SavePolicy) -> Self {
        Self {
            store,
            blobs,
            verifier,
            policy,
        }
    }

    /// [`Verifier::verify`], with the summary's level derived from its XP on the game's curve
    /// ([`gates::with_driver_level`]: ProfileData `Main/Level` is not the driver level), so every
    /// revision, parked bundle and decision the service records carries the driver level.
    async fn verify(&self, bytes: Arc<[u8]>) -> Result<Verified, SaveError> {
        // CR-S105: a line verifies only its own format (FH2's answer for anything else unchanged).
        if !self.policy.format.admits(&bytes) {
            return Err(SaveError::BundleInvalid(BundleError::UnknownFormat));
        }
        let mut verified = self.verifier.verify(bytes).await?;
        verified.summary = gates::with_driver_level(&self.policy.gates.rules, verified.summary);
        Ok(verified)
    }

    /// The store (heads, history, leases).
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Puts a blob, then parks it; puts it again if parking created its row (see the module docs).
    /// Refused with [`SaveError::TooManyParked`], before anything is stored, when the profile's
    /// player already has [`SavePolicy::max_parked`] unexpired bundles parked (a soft cap: two
    /// parallel requests of the lease holder at 19 may both land).
    async fn park_bytes(&self, req: ParkRequest, bytes: &[u8]) -> Result<ParkedRecord, SaveError> {
        if req.reason.sent_by_player() {
            // Only bundles still within their retention count: an expired one (or one kept past
            // it because an accepted revision or a strike names it) frees its slot.
            let now = self.store.head(req.profile).await?.now;
            let parked = self
                .store
                .parked(req.profile)
                .await?
                .iter()
                .filter(|p| p.request.reason.sent_by_player() && p.expires_at > now)
                .count();
            if parked >= self.policy.max_parked {
                return Err(SaveError::TooManyParked { parked });
            }
        }
        self.blobs.put(req.blob.sha256, bytes).await?;
        let parked = self.store.park(req).await?;
        if parked.blob_created {
            self.blobs.put(parked.request.blob.sha256, bytes).await?;
        }
        Ok(parked)
    }

    /// The answer to a commit whose head is not `expected`: the replay of this very commit if its
    /// key already committed it, else why the caller cannot write (lease first, as the store
    /// answers), else a conflict. Nothing is committed.
    async fn moved(
        &self,
        head: &HeadView,
        session: PlaySessionId,
        token: FenceToken,
        expected: Revision,
        commit_id: CommitId,
        blob: Digest,
    ) -> Result<RevisionRecord, SaveError> {
        if let Some(done) = self.store.find_commit(commit_id).await? {
            return if done.profile == head.profile && done.blob.sha256 == blob {
                Ok(done)
            } else {
                Err(SaveError::IdempotencyKeyReused)
            };
        }
        lease::check_writer(line_state(head), session, token, head.now)?;
        Err(SaveError::RevisionConflict {
            expected,
            head: head.revision,
        })
    }

    /// The revision before the line's last reset whose content `content` is, if any: a career a
    /// reset replaced never comes back through an upload. Content equal to the reset revision's
    /// own (a career reset to the fresh career a line once started from) is not a replaced career.
    async fn replaced_by_reset(
        &self,
        head: &HeadView,
        content: Digest,
    ) -> Result<Option<Revision>, SaveError> {
        if head.reset_revision.is_none() {
            return Ok(None);
        }
        let from_reset = self
            .store
            .history(
                head.profile,
                Some(head.reset_revision.next()),
                HISTORY_SEARCH + 1,
            )
            .await?;
        let reset_content = from_reset
            .first()
            .filter(|r| r.revision == head.reset_revision)
            .map(|r| r.content);
        if reset_content == Some(content) {
            return Ok(None);
        }
        Ok(from_reset
            .into_iter()
            .find(|r| r.revision < head.reset_revision && r.content == content)
            .map(|r| r.revision))
    }

    /// Validates, gates, stores and commits a player's bundle. See the module docs for the order.
    pub async fn upload(&self, req: UploadRequest) -> Result<Committed, SaveError> {
        self.upload_assessed(req).await.result
    }

    /// [`SaveService::upload`] with the guardrails' assessment the decision was made with (the
    /// caller records it: `integrity_assessment`, CLOUD_SAVES.md §7.3).
    pub async fn upload_assessed(&self, req: UploadRequest) -> UploadOutcome {
        let mut assessment = None;
        let result = self.upload_in(req, &mut assessment).await;
        // Kept only with the decision it made: a commit, or the refusal it caused.
        let decided = match &result {
            Ok(c) => !c.replayed,
            Err(SaveError::GateRefused { score, .. }) => score.is_some(),
            Err(_) => false,
        };
        UploadOutcome {
            result,
            assessment: assessment.filter(|_| decided),
        }
    }

    async fn upload_in(
        &self,
        mut req: UploadRequest,
        assessed: &mut Option<fi_guard::Assessment>,
    ) -> Result<Committed, SaveError> {
        let replay_key = Some(req.commit_id);
        let checked = self.check_in(&mut req, replay_key).await?;
        let Checked {
            blob,
            bytes,
            verified,
            assessment,
            verdict,
            ..
        } = checked;
        let verdict = match verdict {
            Verdict::Replayed(record) => {
                // Create-only: a present object is untouched; a collected one comes back.
                self.blobs.put(blob.sha256, &bytes).await?;
                return Ok(Committed {
                    record,
                    replayed: true,
                    blob_created: false,
                });
            }
            v => v,
        };
        let verified = verified.expect("every verdict but a replay verified the bundle");
        // A decision already parked and recorded (the same bytes on the same head) records nothing new.
        *assessed = assessment;
        let park = |reason, detail: String| ParkRequest {
            profile: req.profile,
            blob,
            content: verified.content,
            reason,
            based_on: Some(req.expected),
            session: Some(req.session),
            device: Some(req.device),
            summary: Some(verified.summary),
            detail,
            retain_ms: self.policy.park_retention_ms,
        };
        match verdict {
            Verdict::Replayed(_) => unreachable!("answered above"),
            Verdict::Unreadable { again } => {
                let id = match again {
                    Some(id) => id,
                    None => {
                        self.park_bytes(
                            park(ParkReason::Unreadable, "progress numbers unreadable".into()),
                            &bytes,
                        )
                        .await?
                        .id
                    }
                };
                Err(SaveError::SummaryUnreadable { parked: id })
            }
            Verdict::Refused {
                again,
                reason,
                rule,
                score,
                reasons,
            } => {
                let parked = match again {
                    Some(id) => id,
                    None => {
                        self.park_bytes(park(ParkReason::GateRefused, reason.clone()), &bytes)
                            .await?
                            .id
                    }
                };
                Err(SaveError::GateRefused {
                    reason,
                    parked,
                    rule,
                    score,
                    reasons,
                })
            }
            Verdict::Pass {
                summary,
                seed: Some(check),
                ..
            } => {
                self.commit_seed(&req, blob, &verified, summary, &bytes, check)
                    .await
            }
            Verdict::Pass {
                summary,
                gated_base,
                guard_input,
                seed: None,
            } => {
                self.commit_bytes(
                    &req,
                    blob,
                    &verified,
                    summary,
                    &bytes,
                    gated_base,
                    guard_input,
                )
                .await
            }
        }
    }

    /// The dry run of a player's upload (`POST .../save/upload-check`, CR-S71, SAVES_API.md
    /// §4.11): steps 1-5 of the module docs exactly as [`Self::upload`] runs them (the same
    /// function), and nothing else. Nothing is stored: no blob, no revision, no parked bundle, no
    /// assessment. No lease is checked (`req.session` and `req.token` are not read: the real
    /// upload still needs the live lease), `req.commit_id` is not looked up (a dry run is never a
    /// replay, so a head other than `If-Match` is `SAVES.REVISION_CONFLICT`) and `req.device` and
    /// `req.game_sync` are not read.
    ///
    /// `Err` is what the upload would answer before verification (and its other errors, such as
    /// [`SaveError::DigestMismatch`] or [`SaveError::Unavailable`]); the bundle's own verdict is
    /// `Ok`: [`UploadVerdict::Pass`], or [`UploadVerdict::Refused`] with the code, rule, reason,
    /// score and reasons the upload's refusal carries (without its `parkedId`, as nothing is
    /// parked). An invalid bundle is `Refused` with `SAVES.BUNDLE_INVALID`.
    pub async fn check_upload(&self, mut req: UploadRequest) -> Result<UploadVerdict, SaveError> {
        let checked = match self.check_in(&mut req, None).await {
            Ok(checked) => checked,
            Err(SaveError::BundleInvalid(BundleError::TooLarge)) => {
                return Err(SaveError::BundleInvalid(BundleError::TooLarge));
            }
            Err(e @ SaveError::BundleInvalid(_)) => {
                return Ok(UploadVerdict::Refused {
                    code: e.code(),
                    rule: None,
                    reason: e.to_string(),
                    score: None,
                    reasons: Vec::new(),
                });
            }
            Err(e) => return Err(e),
        };
        Ok(match checked.verdict {
            Verdict::Replayed(_) => unreachable!("a dry run looks up no commit"),
            Verdict::Unreadable { .. } => {
                let e = SaveError::SummaryUnreadable {
                    parked: Uuid::nil(),
                };
                UploadVerdict::Refused {
                    code: e.code(),
                    rule: None,
                    reason: e.to_string(),
                    score: None,
                    reasons: Vec::new(),
                }
            }
            Verdict::Refused {
                reason,
                rule,
                score,
                reasons,
                ..
            } => UploadVerdict::Refused {
                code: "SAVES.GATE_REFUSED",
                rule: Some(rule),
                reason,
                score,
                reasons,
            },
            Verdict::Pass { summary, .. } => UploadVerdict::Pass { summary },
        })
    }

    /// Steps 1-5 of an upload (the module docs), shared by [`Self::upload`] and
    /// [`Self::check_upload`]: every refusal and gate, reading only (the head, the history, the
    /// parked bundles). `replay_key` is the upload's `Idempotency-Key`, looked up when the head
    /// is not `If-Match` (a lost reply's retry), and the caller's lease is checked; `None` for a
    /// dry run, which holds no lease (CR-S71: an idle launcher asks before it restores) and whose
    /// moved head is `SAVES.REVISION_CONFLICT`. Every other step is the same.
    async fn check_in(
        &self,
        req: &mut UploadRequest,
        replay_key: Option<CommitId>,
    ) -> Result<Checked, SaveError> {
        if !req.kind.is_player_upload() {
            return Err(SaveError::InvalidArgument(
                "an upload is an upload, a keep-local resolution, an offline import or a fresh start",
            ));
        }
        if req.bytes.len() > self.policy.format.max_bundle() {
            return Err(SaveError::BundleInvalid(BundleError::TooLarge));
        }
        let sha256 = Digest::of(&req.bytes);
        if req
            .declared_sha256
            .is_some_and(|declared| declared != sha256)
        {
            return Err(SaveError::DigestMismatch);
        }
        let blob = BlobRef {
            sha256,
            size: req.bytes.len() as u32,
            format: SaveFormat::of_magic(&req.bytes),
        };
        let head = self.store.head(req.profile).await?;
        if head.revision != req.expected {
            let record = match replay_key {
                Some(commit_id) => {
                    self.moved(
                        &head,
                        req.session,
                        req.token,
                        req.expected,
                        commit_id,
                        sha256,
                    )
                    .await?
                }
                // A dry run holds no lease (CR-S71): the moved head is the answer.
                None => {
                    return Err(SaveError::RevisionConflict {
                        expected: req.expected,
                        head: head.revision,
                    });
                }
            };
            return Ok(Checked {
                blob,
                bytes: Arc::from(std::mem::take(&mut req.bytes)),
                verified: None,
                assessment: None,
                verdict: Verdict::Replayed(record),
            });
        }
        if replay_key.is_some() {
            lease::check_writer(line_state(&head), req.session, req.token, head.now)?;
        }
        let gated = head.mode.is_gated();
        // A Hardcore line at revision 0 starts only as its start says (CR-S51, SAVES_API.md §4.9):
        // a `fresh_start` on a `fresh_start` line; nothing a player uploads on a seed line.
        let fresh_check = match req.kind {
            CommitKind::ImportOffline if gated => return Err(SaveError::HardcoreNeedsFreshCareer),
            CommitKind::ImportOffline if !head.revision.is_none() => {
                return Err(SaveError::InvalidArgument(
                    "an offline import starts an empty noob line",
                ));
            }
            CommitKind::FreshStart if !gated || !head.revision.is_none() => {
                return Err(SaveError::InvalidArgument(
                    "a fresh start is a hardcore line's first revision",
                ));
            }
            CommitKind::FreshStart => match req.fresh_start {
                Some(check) => Some(check),
                None => return Err(SaveError::HardcoreNeedsFreshCareer),
            },
            // CR-S52 (SAVES_API.md §4.10): only where the server allows it.
            CommitKind::SeedFromProfile if !gated || !head.revision.is_none() => {
                return Err(SaveError::InvalidArgument(
                    "a seed from a profile is a hardcore line's first revision",
                ));
            }
            CommitKind::SeedFromProfile if req.seed_from_profile.is_none() => {
                return Err(SaveError::HardcoreNeedsFreshCareer);
            }
            CommitKind::SeedFromProfile => None,
            _ if gated && head.revision.is_none() => {
                return Err(SaveError::HardcoreNeedsFreshCareer);
            }
            _ => None,
        };
        let bytes: Arc<[u8]> = Arc::from(std::mem::take(&mut req.bytes));
        let verified = self.verify(bytes.clone()).await?;
        let checked = |verdict, assessment| Checked {
            blob,
            bytes: bytes.clone(),
            verified: Some(verified),
            assessment,
            verdict,
        };
        if !gated {
            return Ok(checked(
                Verdict::Pass {
                    summary: verified.summary,
                    gated_base: None,
                    guard_input: None,
                    seed: None,
                },
                None,
            ));
        }
        // The line's parked bundles: a refusal of these very bytes on this head is answered
        // again without parking twice or recording a second assessment (review G2-11), and
        // the facts other refusals by score judged against this head are not scored again
        // (review G2-6).
        let parked = self.store.parked(req.profile).await?;
        let same = |reason: ParkReason| {
            parked
                .iter()
                .find(|p| {
                    p.request.blob.sha256 == sha256
                        && p.request.based_on == Some(req.expected)
                        && p.request.reason == reason
                })
                .map(|p| p.id)
        };
        if !verified.summary.is_complete_for_gates() {
            return Ok(checked(
                Verdict::Unreadable {
                    again: same(ParkReason::Unreadable),
                },
                None,
            ));
        }
        let refused_again = same(ParkReason::GateRefused);
        // What a decision records: nothing when these bytes were already refused on this head.
        let recorded = |a: Option<fi_guard::Assessment>| a.filter(|_| refused_again.is_none());
        let refused = |reason: String, rule, score, reasons| Verdict::Refused {
            again: refused_again,
            reason,
            rule,
            score,
            reasons,
        };
        if let (CommitKind::SeedFromProfile, Some(check)) = (req.kind, req.seed_from_profile) {
            // The seed check (CLOUD_SAVES.md §7.6) instead of the guardrails: the save alone,
            // as a whole career from zero over the source's play time. A refusal parks the
            // bundle (NOT_PLAUSIBLE_SEED with the check's reasons); its assessment is the
            // caller's to record with the source profile.
            let facts = (check.facts)(&bytes);
            let verdict = seed::check(
                &self.policy.gates,
                &verified.summary,
                facts.as_ref().map_err(|e| *e),
                check.hours,
            );
            let assessment = recorded(verdict.assessment.clone());
            if !verdict.accepted() {
                let reason = format!("not a plausible seed: {}", verdict.reasons.join(", "));
                return Ok(checked(
                    refused(
                        reason,
                        seed::NOT_PLAUSIBLE_SEED,
                        verdict.assessment.as_ref().map(|a| a.score),
                        verdict.reasons,
                    ),
                    assessment,
                ));
            }
            return Ok(checked(
                Verdict::Pass {
                    summary: verdict.summary,
                    gated_base: Some(req.expected),
                    guard_input: None,
                    seed: Some(check),
                },
                assessment,
            ));
        }
        if let Some(check) = fresh_check {
            // The game's limits first, as for every Hardcore upload (review M2: a number out
            // of range never anchors a line), then the fresh-start check instead of the
            // guardrails (it has no base, CLOUD_SAVES.md §7.4); the garage count it read is
            // the revision's `cars`.
            if let Err(refusal) = gates::check_bounds(&self.policy.gates, &verified.summary) {
                return Ok(checked(
                    refused(refusal.to_string(), refusal.code(), None, Vec::new()),
                    None,
                ));
            }
            let facts = (check.facts)(&bytes);
            let reasons = fresh::check(
                &check.limits,
                &verified.summary,
                facts.as_ref().map_err(|e| *e),
            );
            if !reasons.is_empty() {
                let reason = format!("not a new career: {}", reasons.join(", "));
                return Ok(checked(
                    refused(reason, fresh::NOT_FRESH_START, None, reasons),
                    None,
                ));
            }
            return Ok(checked(
                Verdict::Pass {
                    summary: SaveSummary {
                        cars: facts.ok().map(|f| f.cars),
                        ..verified.summary
                    },
                    gated_base: Some(head.revision),
                    guard_input: None,
                    seed: None,
                },
                None,
            ));
        }
        if let Some(replaced) = self.replaced_by_reset(&head, verified.content).await? {
            let reason = format!(
                "this is revision {replaced}, which the reset of revision {} replaced",
                head.reset_revision
            );
            return Ok(checked(
                refused(reason, REPLACED_BY_RESET, None, Vec::new()),
                None,
            ));
        }
        if let Err(refusal) = gates::check_bounds(&self.policy.gates, &verified.summary) {
            return Ok(checked(
                refused(refusal.to_string(), refusal.code(), None, Vec::new()),
                None,
            ));
        }
        // Hybrid Hardcore checks (owner 2026-10-26 and 2026-10-27, CLOUD_SAVES.md §7.5): of
        // the guardrails, only the fact checks and the one-step ceilings run here (no rate
        // replay), and either refuses whatever the enforcement switch: a ceiling, or a fact a
        // refusal has not judged yet (score 60 each, `ANOMALY_SCORE`). The replayed rates, the
        // day ceilings and the rest of the score are the nightly pass's ([`guardrails`],
        // `fi-cli integrity-nightly`), on the revision this commit records.
        let (base, base_play_ms, _) = gate_base(
            &self.store,
            req.profile,
            head.revision,
            head.current.as_ref(),
        )
        .await?;
        let judged = judged_against(
            &self.policy.gates,
            &parked,
            &base,
            base_play_ms,
            head.play_ms,
            req.expected,
            sha256,
            None,
        );
        let facts = gates::inline_checks(
            &self.policy.gates,
            &base,
            base_play_ms,
            &verified.summary,
            head.play_ms,
            judged,
        );
        if let Some(refusal) = facts.refusal(true) {
            let score = facts.score;
            let reasons: Vec<&'static str> = facts.reasons.iter().map(|r| r.code()).collect();
            let refusal = gates::GateRefusal::Guard(refusal);
            return Ok(checked(
                refused(refusal.to_string(), refusal.code(), Some(score), reasons),
                recorded(Some(facts)),
            ));
        }
        Ok(checked(
            Verdict::Pass {
                summary: verified.summary,
                gated_base: Some(head.revision),
                guard_input: Some(GuardInput {
                    game: req.game_sync,
                    play_ms: Some(head.play_ms),
                }),
                seed: None,
            },
            None,
        ))
    }

    /// [`Self::commit_bytes`] of an accepted `seed_from_profile` (CR-S52): revision 1 on the empty
    /// line its check measured, naming its source profile.
    async fn commit_seed(
        &self,
        req: &UploadRequest,
        blob: BlobRef,
        verified: &Verified,
        summary: SaveSummary,
        bytes: &[u8],
        check: SeedFromProfileCheck,
    ) -> Result<Committed, SaveError> {
        self.blobs.put(blob.sha256, bytes).await?;
        let committed = self
            .store
            .commit(CommitRequest {
                profile: req.profile,
                session: req.session,
                token: req.token,
                expected: req.expected,
                commit_id: req.commit_id,
                blob,
                content: verified.content,
                kind: req.kind,
                actor: Actor::Player,
                device: Some(req.device),
                client_label: Some(verified.label_hex()),
                restored_from: None,
                summary,
                gated_base: Some(req.expected),
                parked_from: None,
                guard_input: None,
                seed_source: Some(check.source),
            })
            .await?;
        if committed.blob_created {
            self.blobs.put(blob.sha256, bytes).await?;
        }
        Ok(committed)
    }

    /// Steps 6-8 of an upload: the blob put, the commit, the re-put.
    #[allow(clippy::too_many_arguments)]
    async fn commit_bytes(
        &self,
        req: &UploadRequest,
        blob: BlobRef,
        verified: &Verified,
        summary: SaveSummary,
        bytes: &[u8],
        gated_base: Option<Revision>,
        guard_input: Option<GuardInput>,
    ) -> Result<Committed, SaveError> {
        self.blobs.put(blob.sha256, bytes).await?;
        let committed = self
            .store
            .commit(CommitRequest {
                profile: req.profile,
                session: req.session,
                token: req.token,
                expected: req.expected,
                commit_id: req.commit_id,
                blob,
                content: verified.content,
                kind: req.kind,
                actor: Actor::Player,
                device: Some(req.device),
                client_label: Some(verified.label_hex()),
                restored_from: None,
                summary,
                gated_base,
                parked_from: None,
                guard_input,
                seed_source: None,
            })
            .await?;
        if committed.blob_created {
            self.blobs.put(blob.sha256, bytes).await?;
        }
        Ok(committed)
    }

    /// Keeps a PC's save that lost a conflict ("keep cloud") on the server too, so a wrong choice
    /// can be undone from the portal or by support. Structurally broken bundles are kept as well
    /// (reason `unreadable`): nothing a player's PC held is ever thrown away by the server.
    pub async fn park_conflict_loser(
        &self,
        profile: ProfileId,
        session: PlaySessionId,
        device: DeviceId,
        based_on: Revision,
        bytes: Vec<u8>,
    ) -> Result<ParkedRecord, SaveError> {
        if bytes.len() > self.policy.format.max_bundle() {
            return Err(SaveError::BundleInvalid(BundleError::TooLarge));
        }
        let sha256 = Digest::of(&bytes);
        let blob = BlobRef {
            sha256,
            size: bytes.len() as u32,
            format: SaveFormat::of_magic(&bytes),
        };
        let bytes: Arc<[u8]> = Arc::from(bytes);
        let (reason, content, summary, detail) = match self.verify(bytes.clone()).await {
            Ok(v) => (
                ParkReason::ConflictLoser,
                v.content,
                Some(v.summary),
                String::from("kept the cloud save"),
            ),
            Err(SaveError::BundleInvalid(e)) => {
                (ParkReason::Unreadable, sha256, None, e.to_string())
            }
            Err(other) => return Err(other),
        };
        self.park_bytes(
            ParkRequest {
                profile,
                blob,
                content,
                reason,
                based_on: Some(based_on),
                session: Some(session),
                device: Some(device),
                summary,
                detail,
                retain_ms: self.policy.park_retention_ms,
            },
            &bytes,
        )
        .await
    }

    /// Brings an older revision back as a new head. History is never rewritten: the replaced head
    /// stays in history and can itself be restored. A player cannot restore anything a reset
    /// replaced (`SAVES.RESTORE_BEHIND_RESET`); a moderator can.
    pub async fn restore(&self, req: RestoreRequest) -> Result<Committed, SaveError> {
        let source = self.store.revision(req.profile, req.from).await?;
        self.store
            .commit(CommitRequest {
                profile: req.profile,
                session: req.session,
                token: req.token,
                expected: req.expected,
                commit_id: req.commit_id,
                blob: source.blob,
                content: source.content,
                kind: CommitKind::Restore,
                actor: req.actor,
                device: None,
                client_label: None,
                restored_from: Some(req.from),
                summary: source.summary,
                gated_base: None,
                parked_from: None,
                guard_input: None,
                seed_source: None,
            })
            .await
    }

    /// Resets a line: the reset career becomes the newest revision and moves the line's reset
    /// revision, so players can no longer restore or re-upload what it replaced. The replaced
    /// head is parked first as `pre_reset` (a copy for support; the line's history keeps it too).
    /// A player's in-flight upload based on the old head fails with `REVISION_CONFLICT`.
    pub async fn reset(&self, req: ResetRequest) -> Result<Committed, SaveError> {
        if !req.kind.is_reset() {
            return Err(SaveError::InvalidArgument(
                "a reset is a moderation, season or career reset",
            ));
        }
        if req.bytes.len() > self.policy.format.max_bundle() {
            return Err(SaveError::BundleInvalid(BundleError::TooLarge));
        }
        let sha256 = Digest::of(&req.bytes);
        let blob = BlobRef {
            sha256,
            size: req.bytes.len() as u32,
            format: SaveFormat::of_magic(&req.bytes),
        };
        let head = self.store.head(req.profile).await?;
        if head.revision != req.expected {
            let record = self
                .moved(
                    &head,
                    req.session,
                    req.token,
                    req.expected,
                    req.commit_id,
                    sha256,
                )
                .await?;
            return Ok(Committed {
                record,
                replayed: true,
                blob_created: false,
            });
        }
        lease::check_writer(line_state(&head), req.session, req.token, head.now)?;
        let bytes: Arc<[u8]> = Arc::from(req.bytes);
        let verified = self.verify(bytes.clone()).await?;
        if let Some(current) = &head.current {
            self.store
                .park(ParkRequest {
                    profile: req.profile,
                    blob: current.blob,
                    content: current.content,
                    reason: ParkReason::PreReset,
                    based_on: Some(head.revision),
                    session: Some(req.session),
                    device: None,
                    summary: Some(current.summary),
                    detail: format!("replaced by a {} reset", req.kind.as_str()),
                    retain_ms: self.policy.park_retention_ms,
                })
                .await?;
        }
        self.blobs.put(sha256, &bytes).await?;
        let committed = self
            .store
            .commit(CommitRequest {
                profile: req.profile,
                session: req.session,
                token: req.token,
                expected: req.expected,
                commit_id: req.commit_id,
                blob,
                content: verified.content,
                kind: req.kind,
                actor: req.actor,
                device: None,
                client_label: Some(verified.label_hex()),
                restored_from: None,
                summary: verified.summary,
                gated_base: None,
                parked_from: None,
                guard_input: None,
                seed_source: None,
            })
            .await?;
        if committed.blob_created {
            self.blobs.put(sha256, &bytes).await?;
        }
        Ok(committed)
    }

    /// A moderator accepts a parked bundle (a refusal reviewed and overturned): it becomes the
    /// newest revision (`parked_accept`, ungated: a person decided). Only bundles that verify can
    /// become a revision; an `unreadable` one stays parked for support to inspect.
    pub async fn accept_parked(&self, req: AcceptParkedRequest) -> Result<Committed, SaveError> {
        let parked = self
            .store
            .parked(req.profile)
            .await?
            .into_iter()
            .find(|p| p.id == req.parked)
            .ok_or(SaveError::InvalidArgument("no such parked bundle"))?;
        let blob = parked.request.blob;
        let bytes = self.blobs.get(blob.sha256).await?;
        if Digest::of(&bytes) != blob.sha256 {
            return Err(SaveError::Unavailable(format!(
                "parked bundle {} failed its integrity check",
                blob.sha256.hex()
            )));
        }
        let verified = self.verify(Arc::from(bytes)).await?;
        self.store
            .commit(CommitRequest {
                profile: req.profile,
                session: req.session,
                token: req.token,
                expected: req.expected,
                commit_id: req.commit_id,
                blob,
                content: verified.content,
                kind: CommitKind::ParkedAccept,
                actor: Actor::Moderator,
                device: parked.request.device,
                client_label: Some(verified.label_hex()),
                restored_from: None,
                summary: verified.summary,
                gated_base: None,
                parked_from: Some(parked.id),
                guard_input: None,
                seed_source: None,
            })
            .await
    }

    /// A revision's record and bytes, re-hashed: a stored bundle that no longer matches its digest
    /// is reported, never served.
    pub async fn download(
        &self,
        profile: ProfileId,
        revision: Revision,
    ) -> Result<(RevisionRecord, Vec<u8>), SaveError> {
        let record = self.store.revision(profile, revision).await?;
        let bytes = self.blobs.get(record.blob.sha256).await?;
        if Digest::of(&bytes) != record.blob.sha256 {
            return Err(SaveError::Unavailable(format!(
                "stored bundle {} failed its integrity check",
                record.blob.sha256.hex()
            )));
        }
        Ok((record, bytes))
    }
}

/// The numbers, play time and revision a Hardcore upload based on `head` is measured against: the
/// newest revision at or below `head` whose numbers are readable, else zero at the start of the
/// line (fail closed; no revision). Reads only revisions at or below `head`, which never change,
/// so the nightly pass reads what the upload would have.
async fn gate_base<S: SaveStore>(
    store: &S,
    profile: ProfileId,
    head: Revision,
    current: Option<&RevisionRecord>,
) -> Result<(SaveSummary, u64, Option<Revision>), SaveError> {
    if head.is_none() {
        return Ok((SaveSummary::ZERO, 0, None));
    }
    // The upload path holds the head's record already (`HeadView::current`): no read when it is
    // readable.
    if let Some(r) = current.filter(|r| r.revision == head && r.summary.is_complete_for_gates()) {
        return Ok((r.summary, r.play_ms, Some(r.revision)));
    }
    let older = store
        .history(profile, Some(head.next()), HISTORY_SEARCH + 1)
        .await?;
    Ok(older
        .into_iter()
        .find(|r| r.summary.is_complete_for_gates())
        .map_or((SaveSummary::ZERO, 0, None), |r| {
            (r.summary, r.play_ms, Some(r.revision))
        }))
}

/// The fact checks' values the line's parked refusals by score judged against `head` (review
/// G2-6), other bytes than `blob` (the same bundle sent again is decided again the same way,
/// review G2-11), parked no later than `until` when given (the nightly pass: the revision's
/// commit time, so a refusal parked after the revision landed is not one its upload saw).
#[allow(clippy::too_many_arguments)]
fn judged_against(
    policy: &GatePolicy,
    parked: &[ParkedRecord],
    base: &SaveSummary,
    base_play_ms: u64,
    play_ms: u64,
    head: Revision,
    blob: Digest,
    until: Option<UnixMs>,
) -> Judged {
    parked
        .iter()
        .filter(|p| {
            p.request.reason == ParkReason::GateRefused
                && p.request.based_on == Some(head)
                && p.request.blob.sha256 != blob
                && until.is_none_or(|t| p.parked_at <= t)
        })
        .filter_map(|p| {
            let summary = p.request.summary.filter(|s| s.is_complete_for_gates())?;
            let a = gates::assess(
                policy,
                base,
                base_play_ms,
                &summary,
                play_ms,
                gates::Carried::default(),
                None,
            );
            Some(refused_facts(&p.request.detail, &a))
        })
        .fold(Judged::default(), Judged::merge)
}

/// One gated upload, as the guardrails measure it ([`guardrails`]).
#[derive(Clone, Copy, Debug)]
pub struct GuardrailsInput<'a> {
    /// Whose line.
    pub profile: ProfileId,
    /// The head the upload was based on (its `If-Match`, the commit's `gated_base`).
    pub head: Revision,
    /// The line's play time at the upload (the head's, as read at the upload; for a committed
    /// revision, its `play_ms`).
    pub play_ms: u64,
    /// The upload's numbers.
    pub next: &'a SaveSummary,
    /// Its bundle's SHA-256: its own parked refusal on `head` judges no fact (review G2-11).
    pub blob: Digest,
    /// The profile's last accepted game sync at the upload.
    pub game: Option<fi_guard::GameSync>,
    /// Parked refusals later than this are not read (the nightly pass: the revision's commit
    /// time); `None`: every one.
    pub parked_until: Option<UnixMs>,
}

/// The guardrails' full assessment of one gated Hardcore upload (CLOUD_SAVES.md §7.3): the base
/// ([`gate_base`]), the facts its parked refusals judged, the rates and level high-water mark
/// replayed from the line's history ([`carried`]), the game sync. This is the assessment the
/// upload path made inline before the hybrid checks (owner 2026-10-26) and the one the nightly
/// pass makes now ([`nightly_assessment`]): it reads only revisions at or below `head` and the
/// parked bundles, so made later it measures the same.
pub async fn guardrails<S: SaveStore>(
    store: &S,
    policy: &GatePolicy,
    input: GuardrailsInput<'_>,
) -> Result<fi_guard::Assessment, SaveError> {
    let (base, base_play_ms, base_revision) =
        gate_base(store, input.profile, input.head, None).await?;
    let parked = store.parked(input.profile).await?;
    let judged = judged_against(
        policy,
        &parked,
        &base,
        base_play_ms,
        input.play_ms,
        input.head,
        input.blob,
        input.parked_until,
    );
    let carried = carried(
        store,
        policy,
        input.profile,
        base_revision,
        &base,
        base_play_ms,
        &[judged],
    )
    .await?;
    Ok(gates::assess(
        policy,
        &base,
        base_play_ms,
        input.next,
        input.play_ms,
        carried,
        input.game,
    ))
}

/// The nightly pass's assessment of a committed revision (CLOUD_SAVES.md §7.5): [`guardrails`]
/// on the head it was based on (the revision before it: a gated commit lands only on its
/// `gated_base`), at the play time the upload measured (recorded in [`GuardInput::play_ms`]; the
/// revision's own when absent), with the game sync its commit recorded and the parked
/// refusals that existed when it landed. `None` for a revision the pass does not assess (the
/// line's first, which the fresh-start check or the seed made).
pub async fn nightly_assessment<S: SaveStore>(
    store: &S,
    policy: &GatePolicy,
    record: &RevisionRecord,
    input: GuardInput,
) -> Result<Option<fi_guard::Assessment>, SaveError> {
    if record.revision.get() < 2
        || !matches!(
            record.kind,
            CommitKind::Upload | CommitKind::ResolveKeepLocal
        )
    {
        return Ok(None);
    }
    guardrails(
        store,
        policy,
        GuardrailsInput {
            profile: record.profile,
            head: Revision::new(record.revision.get() - 1),
            play_ms: input.play_ms.unwrap_or(record.play_ms),
            next: &record.summary,
            blob: record.blob.sha256,
            game: input.game,
            parked_until: Some(record.committed_at),
        },
    )
    .await
    .map(Some)
}

/// What the line carries at its gated base (`base`, read with `summary` at `play_ms`) into the
/// next measurement ([`gates::Carried`]). A save line stores no rate, so the rates are
/// replayed from its history back to the newest rate anchor (a fresh career, a Beta import, a
/// reset, an accepted parked bundle: zero there; a restore is an ordinary step, review G2-2),
/// and no further than the newest revision below [`SLOW_WINDOW_MS`] of play, whose step into
/// the window is counted (review G2-5). Three measures, the higher kept metric by metric: the
/// exact replay of the newest [`HISTORY_SEARCH`] of those revisions, and one step over each
/// window ([`RATE_WINDOW_MS`] for the fast rates, [`SLOW_WINDOW_MS`] for the slow ones) from
/// its first revision ([`SaveStore::play_window_start`]), so padding the history with many
/// small commits cannot push a gain out of a rate. A window step spans many measurements, so
/// its fast rates count its whole hours. The level high-water mark follows the replay and
/// restarts at a restore ([`SaveStore::level_high_water`]); the judged facts are those of the
/// parked refusals by score against the line's head (`refused`, other bytes than this upload's:
/// the same bundle sent again is decided again the same way, review G2-11).
async fn carried<S: SaveStore>(
    store: &S,
    policy: &GatePolicy,
    profile: ProfileId,
    base: Option<Revision>,
    summary: &SaveSummary,
    play_ms: u64,
    refused: &[Judged],
) -> Result<gates::Carried, SaveError> {
    let judged = refused.iter().fold(Judged::default(), |a, b| a.merge(*b));
    let Some(base) = base else {
        return Ok(gates::Carried {
            judged,
            ..gates::Carried::default()
        });
    };
    let rules = &policy.rules;
    let floor = play_ms.saturating_sub(SLOW_WINDOW_MS);
    let mut chain: Vec<RevisionRecord> = Vec::new();
    for r in store
        .history(profile, Some(base.next()), HISTORY_SEARCH)
        .await?
    {
        let stop = r.kind.is_rate_anchor() || r.play_ms < floor;
        if r.summary.is_complete_for_gates() {
            chain.push(r);
            if stop {
                break;
            }
        } else if stop && r.kind.is_rate_anchor() {
            break;
        }
    }
    chain.reverse();
    let mut high = match chain.first() {
        Some(first) => store.level_high_water(profile, first.revision).await?,
        None => None,
    };
    let mut fine = Rates::default();
    for pair in chain.windows(2) {
        let (from, to) = (
            gates::numbers(rules, &pair[0].summary),
            gates::numbers(rules, &pair[1].summary),
        );
        fine = fine.advance(
            &from,
            &to,
            gates::play_hours(policy, pair[0].play_ms, pair[1].play_ms),
            rules,
            high,
        );
        // Levels derived from the XP (stored pre-2026-10-31 rows hold `Main/Level`). The store's
        // own high-water marks (`SaveStore::level_high_water`) still take the stored level, so a
        // mark over such rows can sit below the true one: after a level drop it grants more spins
        // than the true mark would (a slight loosening of G2-3 over old history only; never a
        // false refusal). Logged in REVIEW_LOG "A save's level".
        high = if pair[1].kind.restarts_level_high() {
            to.level
        } else {
            high.max(from.level).max(to.level)
        };
    }
    let level_high = match chain.last() {
        Some(last) if last.revision == base => high,
        _ => store.level_high_water(profile, base).await?,
    };
    let mut rates = fine;
    for window in [RATE_WINDOW_MS, SLOW_WINDOW_MS] {
        let start = store
            .play_window_start(profile, base, play_ms.saturating_sub(window))
            .await?;
        if let Some(first) = start.filter(|f| f.revision < base) {
            let mark = store.level_high_water(profile, first.revision).await?;
            rates = rates.max(Rates::default().advance_window(
                &gates::numbers(rules, &first.summary),
                &gates::numbers(rules, summary),
                gates::play_hours(policy, first.play_ms, play_ms),
                rules,
                mark,
            ));
        }
    }
    Ok(gates::Carried {
        rates,
        level_high,
        judged,
    })
}

/// The facts a parked refusal by score judged (`detail`, the refusal's reason: "anomaly score N
/// (CODE, ...)"; [`fi_guard::Refusal::reason`]), measured again as `assessment` against the
/// line's gated base: only the fact checks among its codes count, and a ceiling's refusal (no
/// codes) judged none.
fn refused_facts(detail: &str, assessment: &fi_guard::Assessment) -> Judged {
    let Some(codes) = detail
        .strip_prefix("anomaly score ")
        .and_then(|rest| rest.split_once('('))
        .map(|(_, codes)| codes.trim_end_matches(')'))
    else {
        return Judged::default();
    };
    let had = |code: fi_guard::ReasonCode| codes.split(", ").any(|c| c == code.code());
    let j = assessment.judged;
    Judged {
        level_deviation: if had(fi_guard::ReasonCode::LevelXpMismatch) {
            j.level_deviation
        } else {
            0
        },
        token_deviation: if had(fi_guard::ReasonCode::TokensSkillMismatch) {
            j.token_deviation
        } else {
            0
        },
        drop_floor: j
            .drop_floor
            .filter(|_| had(fi_guard::ReasonCode::XpLevelDrop)),
    }
}
