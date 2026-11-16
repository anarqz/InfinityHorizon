//! Save errors with stable codes (docs/contracts/SAVES_API.md §6). The codes are the contract;
//! messages are for logs and may change. Every code is also listed in `fi_protocol::ErrorCode`
//! with its HTTP status, and a test in services/api checks the two lists agree.

use crate::bundle::BundleError;
use crate::model::{FenceToken, Revision};
use fi_domain::{DeviceId, UnixMs};
use uuid::Uuid;

/// A refused or failed save operation.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SaveError {
    /// The profile has no save line.
    #[error("profile has no save line")]
    ProfileNotFound,
    /// Another session holds a live lease; the player may take over.
    #[error("the save is in use by another session until {expires_at}")]
    LeaseHeld {
        /// The holder's device.
        device: DeviceId,
        /// The holder's PC name, for the "in use on ..." prompt.
        device_label: String,
        /// When the holder's lease lapses unless renewed.
        expires_at: UnixMs,
    },
    /// The caller is not the current holder: its lease was taken over, released, or expired and
    /// granted again. Its writes are fenced off for good; it must start a new session.
    #[error("this session no longer holds the save lease")]
    LeaseLost {
        /// The token now in force, if any.
        current: Option<FenceToken>,
    },
    /// The caller held the lease but let it lapse; nobody took it since. Re-acquiring gives a new
    /// token, and the commit can then be retried with the same expected revision.
    #[error("the save lease lapsed at {expired_at}")]
    LeaseExpired {
        /// When it lapsed.
        expired_at: UnixMs,
    },
    /// The line moved past the revision the client based its save on (`If-Match` failed).
    #[error("expected revision {expected}, the save is at {head}")]
    RevisionConflict {
        /// What the client sent.
        expected: Revision,
        /// The current head.
        head: Revision,
    },
    /// The idempotency key already committed a different save.
    #[error("the idempotency key was used for another commit")]
    IdempotencyKeyReused,
    /// No such revision (or retention dropped it).
    #[error("revision {0} not found")]
    RevisionNotFound(Revision),
    /// The bundle is not a well-formed FH2 save.
    #[error("invalid save bundle: {0}")]
    BundleInvalid(BundleError),
    /// The bytes do not hash to the digest the client declared (`Repr-Digest`).
    #[error("the bundle does not match its declared SHA-256")]
    DigestMismatch,
    /// The Hardcore gates refused the save's progress; it was parked for review.
    #[error("the save's progress was refused: {reason}")]
    GateRefused {
        /// Why, in English with the numbers (no player data).
        reason: String,
        /// The parked copy.
        parked: Uuid,
        /// The stable rule (SAVES_API.md §6): `LIMITS`, `REPLACED_BY_RESET`, a guardrails ceiling
        /// (`CREDITS_CEILING`, `CREDITS_UNEXPLAINED_CEILING`, `WINNINGS_CEILING`, `XP_CEILING`,
        /// `LEVEL_CEILING`, and over a day `CREDITS_UNEXPLAINED_DAY_CEILING`,
        /// `WINNINGS_DAY_CEILING`, `XP_DAY_CEILING`, `LEVEL_DAY_CEILING`) or `ANOMALY_SCORE` (the
        /// enforcement switch on): `fi_guard::CeilingCode::code`; or `NOT_FRESH_START` (CR-S51, a
        /// `fresh_start` over the fresh-start ceilings, [`crate::fresh`]).
        rule: &'static str,
        /// The guardrails' score, when the upload was assessed.
        score: Option<u8>,
        /// Its reason codes (`fi_guard::ReasonCode`), empty when not assessed.
        reasons: Vec<&'static str>,
    },
    /// The server could not read the numbers a Hardcore save needs; it was parked.
    #[error("the save's progress numbers cannot be read")]
    SummaryUnreadable {
        /// The parked copy.
        parked: Uuid,
    },
    /// A Hardcore line starts with the server's fresh career, never with an upload.
    #[error("a hardcore profile starts from the server's fresh career")]
    HardcoreNeedsFreshCareer,
    /// The profile keeps no cloud data now (OD-52: only a Supporter's Hardcore profile does),
    /// checked under the line's lock by a store that enforces it.
    #[error("only a Supporter's hardcore profile keeps a save line")]
    NotEligible {
        /// The account's tier (`vip` | `free`).
        tier: String,
        /// The profile's mode.
        mode: String,
    },
    /// A player restore that would bring back a revision a reset replaced (moderators may).
    #[error("revision {reset} reset this profile; older revisions can only be restored by support")]
    RestoreBehindReset {
        /// The newest reset revision.
        reset: Revision,
    },
    /// A player restore of a revision the nightly guardrails pass flagged, or of a restore whose
    /// chain reaches one (CLOUD_SAVES.md §7.5): a moderator decides (moderators may).
    #[error("revision {revision} is held for moderation and cannot be restored")]
    RestoreFlagged {
        /// The flagged revision.
        revision: Revision,
    },
    /// A malformed argument (TTL out of range, restore of a later revision, ...).
    #[error("invalid argument: {0}")]
    InvalidArgument(&'static str),
    /// Storage failed or timed out. Retryable; nothing was committed.
    #[error("save storage unavailable: {0}")]
    Unavailable(String),
    /// The profile already keeps [`crate::SavePolicy::max_parked`] bundles its player sent
    /// (refused, unreadable, conflict losers): nothing is parked or committed, the bundle stays on
    /// the player's PC (SAVES_API.md §4.6, owner 2026-10-28).
    #[error("too many parked bundles ({parked}); the save stays on this PC")]
    TooManyParked {
        /// How many the profile keeps.
        parked: usize,
    },
}

impl SaveError {
    /// Stable code.
    pub const fn code(&self) -> &'static str {
        match self {
            SaveError::ProfileNotFound => "SAVES.PROFILE_NOT_FOUND",
            SaveError::LeaseHeld { .. } => "SAVES.LEASE_HELD",
            SaveError::LeaseLost { .. } => "SAVES.LEASE_LOST",
            SaveError::LeaseExpired { .. } => "SAVES.LEASE_EXPIRED",
            SaveError::RevisionConflict { .. } => "SAVES.REVISION_CONFLICT",
            SaveError::IdempotencyKeyReused => "SAVES.IDEMPOTENCY_KEY_REUSED",
            SaveError::NotEligible { .. } => "SAVES.NOT_ELIGIBLE",
            SaveError::RevisionNotFound(_) => "SAVES.REVISION_NOT_FOUND",
            SaveError::BundleInvalid(_) => "SAVES.BUNDLE_INVALID",
            SaveError::DigestMismatch => "SAVES.DIGEST_MISMATCH",
            SaveError::GateRefused { .. } => "SAVES.GATE_REFUSED",
            SaveError::SummaryUnreadable { .. } => "SAVES.SUMMARY_UNREADABLE",
            SaveError::HardcoreNeedsFreshCareer => "SAVES.HARDCORE_NEEDS_FRESH_CAREER",
            SaveError::RestoreBehindReset { .. } => "SAVES.RESTORE_BEHIND_RESET",
            SaveError::RestoreFlagged { .. } => "SAVES.RESTORE_FLAGGED",
            SaveError::InvalidArgument(_) => "SAVES.INVALID_ARGUMENT",
            SaveError::Unavailable(_) => "SAVES.UNAVAILABLE",
            SaveError::TooManyParked { .. } => "SAVES.RATE_LIMITED",
        }
    }

    /// Every code, for the parity test with `fi_protocol::ErrorCode`.
    pub const CODES: [&'static str; 17] = [
        "SAVES.PROFILE_NOT_FOUND",
        "SAVES.LEASE_HELD",
        "SAVES.LEASE_LOST",
        "SAVES.LEASE_EXPIRED",
        "SAVES.REVISION_CONFLICT",
        "SAVES.IDEMPOTENCY_KEY_REUSED",
        "SAVES.REVISION_NOT_FOUND",
        "SAVES.BUNDLE_INVALID",
        "SAVES.DIGEST_MISMATCH",
        "SAVES.GATE_REFUSED",
        "SAVES.SUMMARY_UNREADABLE",
        "SAVES.HARDCORE_NEEDS_FRESH_CAREER",
        "SAVES.RESTORE_BEHIND_RESET",
        "SAVES.RESTORE_FLAGGED",
        "SAVES.INVALID_ARGUMENT",
        "SAVES.UNAVAILABLE",
        "SAVES.RATE_LIMITED",
    ];

    /// Whether retrying the same request later can succeed.
    pub const fn is_retryable(&self) -> bool {
        matches!(self, SaveError::Unavailable(_))
    }
}
