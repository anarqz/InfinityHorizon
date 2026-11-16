//! The error envelope and every stable code (ACCOUNTS.md §9, SAVES_API.md §6, PORTAL_API.md §2).

use serde::{Deserialize, Serialize};

/// `{"code": "...", "message": "...", "details": {...}}`. Clients branch on `code`; `message` is
/// English for logs and may change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Stable code, `DOMAIN.CODE`.
    pub code: String,
    /// Human-readable, for logs.
    pub message: String,
    /// Structured context (`expected`/`head`, `deviceLabel`, `retryAfterS`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

macro_rules! codes {
    ($($variant:ident = ($code:literal, $status:literal),)*) => {
        /// Every error code with its HTTP status.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum ErrorCode {
            $(
                #[doc = $code]
                $variant,
            )*
        }

        impl ErrorCode {
            /// Every code, in contract order.
            pub const ALL: &'static [ErrorCode] = &[$(ErrorCode::$variant,)*];

            /// The wire string.
            pub const fn code(self) -> &'static str {
                match self {
                    $(ErrorCode::$variant => $code,)*
                }
            }

            /// The HTTP status it is sent with.
            pub const fn status(self) -> u16 {
                match self {
                    $(ErrorCode::$variant => $status,)*
                }
            }
        }
    };
}

codes! {
    RequestInvalid = ("REQUEST.INVALID", 400),
    RequestIdempotencyKeyReused = ("REQUEST.IDEMPOTENCY_KEY_REUSED", 409),
    RequestTooLarge = ("REQUEST.TOO_LARGE", 413),
    RequestRateLimited = ("REQUEST.RATE_LIMITED", 429),
    AuthTokenInvalid = ("AUTH.TOKEN_INVALID", 401),
    AuthTokenExpired = ("AUTH.TOKEN_EXPIRED", 401),
    AuthInvalidCredentials = ("AUTH.INVALID_CREDENTIALS", 401),
    AuthRefreshInvalid = ("AUTH.REFRESH_INVALID", 401),
    AuthRefreshReused = ("AUTH.REFRESH_REUSED", 401),
    AuthLegacyTokenUnknown = ("AUTH.LEGACY_TOKEN_UNKNOWN", 401),
    AuthLegacyTokenUsed = ("AUTH.LEGACY_TOKEN_USED", 401),
    AuthSessionExpired = ("AUTH.SESSION_EXPIRED", 401),
    AuthForbidden = ("AUTH.FORBIDDEN", 403),
    AuthRateLimited = ("AUTH.RATE_LIMITED", 429),
    AuthIdTokenInvalid = ("AUTH.ID_TOKEN_INVALID", 401),
    AuthReauthRequired = ("AUTH.REAUTH_REQUIRED", 401),
    AuthIdentityTicketInvalid = ("AUTH.IDENTITY_TICKET_INVALID", 401),
    AuthCodeInvalid = ("AUTH.CODE_INVALID", 400),
    AuthAuthorizationPending = ("AUTH.AUTHORIZATION_PENDING", 400),
    AuthEmailUnverified = ("AUTH.EMAIL_UNVERIFIED", 403),
    AuthConfirmationCodeInvalid = ("AUTH.CONFIRMATION_CODE_INVALID", 400),
    AuthProviderNotAllowed = ("AUTH.PROVIDER_NOT_ALLOWED", 403),
    AuthChallengeRequired = ("AUTH.CHALLENGE_REQUIRED", 403),
    AuthChallengeFailed = ("AUTH.CHALLENGE_FAILED", 403),
    AuthUnauthenticated = ("AUTH.UNAUTHENTICATED", 401),
    AuthRoleRequired = ("AUTH.ROLE_REQUIRED", 403),
    AccountsRegistrationDisabled = ("ACCOUNTS.REGISTRATION_DISABLED", 403),
    AccountsBanned = ("ACCOUNTS.BANNED", 403),
    AccountsFrozen = ("ACCOUNTS.FROZEN", 403),
    CrashNotFound = ("CRASH.NOT_FOUND", 404),
    CrashDumpsDisabled = ("CRASH.DUMPS_DISABLED", 409),
    CrashContextInvalid = ("CRASH.CONTEXT_INVALID", 409),
    CrashExpired = ("CRASH.EXPIRED", 410),
    CrashDumpInvalid = ("CRASH.DUMP_INVALID", 422),
    ContentPolicyUnavailable = ("CONTENT.POLICY_UNAVAILABLE", 503),
    ContentRecheckRequired = ("CONTENT.RECHECK_REQUIRED", 403),
    ContentIncompatible = ("CONTENT.INCOMPATIBLE", 403),
    ContentInconclusive = ("CONTENT.INCONCLUSIVE", 403),
    ContentPolicyChanged = ("CONTENT.POLICY_CHANGED", 409),
    AccountsDeletionPending = ("ACCOUNTS.DELETION_PENDING", 403),
    AccountsNotFound = ("ACCOUNTS.NOT_FOUND", 404),
    AccountsNameUnavailable = ("ACCOUNTS.NAME_UNAVAILABLE", 409),
    AccountsProfileLimit = ("ACCOUNTS.PROFILE_LIMIT", 409),
    AccountsCareerExists = ("ACCOUNTS.CAREER_EXISTS", 409),
    AccountsRenameNotAllowed = ("ACCOUNTS.RENAME_NOT_ALLOWED", 409),
    AccountsModeChangeForbidden = ("ACCOUNTS.MODE_CHANGE_FORBIDDEN", 409),
    AccountsModeChangeConfirm = ("ACCOUNTS.MODE_CHANGE_CONFIRM", 409),
    AccountsNotPendingDeletion = ("ACCOUNTS.NOT_PENDING_DELETION", 409),
    AccountsAlreadyLinked = ("ACCOUNTS.ALREADY_LINKED", 409),
    AccountsEmailTaken = ("ACCOUNTS.EMAIL_TAKEN", 409),
    AccountsInvalidName = ("ACCOUNTS.INVALID_NAME", 422),
    AccountsInvalidGamertag = ("ACCOUNTS.INVALID_GAMERTAG", 422),
    AccountsConfirmationMismatch = ("ACCOUNTS.CONFIRMATION_MISMATCH", 422),
    GameWrongPool = ("GAME.WRONG_POOL", 403),
    GameCarDbRefused = ("GAME.CAR_DB_REFUSED", 403),
    GameUpdateRequired = ("GAME.UPDATE_REQUIRED", 403),
    GameNotServed = ("GAME.NOT_SERVED", 404),
    GameForeignXuid = ("GAME.FOREIGN_XUID", 403),
    MmWorldNotFound = ("MM.WORLD_NOT_FOUND", 404),
    MmNotHost = ("MM.NOT_HOST", 403),
    MmAlreadyHosting = ("MM.ALREADY_HOSTING", 409),
    MmGrantInvalid = ("MM.GRANT_INVALID", 401),
    MmAgentUnauthorized = ("MM.AGENT_UNAUTHORIZED", 401),
    SessionsInUseElsewhere = ("SESSIONS.IN_USE_ELSEWHERE", 409),
    SessionsProfileInUse = ("SESSIONS.PROFILE_IN_USE", 409),
    SessionsEnded = ("SESSIONS.ENDED", 409),
    SessionsKicked = ("SESSIONS.KICKED", 403),
    SharingTooManyDevices = ("SHARING.TOO_MANY_DEVICES", 409),
    SharingTooManyRecentDevices = ("SHARING.TOO_MANY_RECENT_DEVICES", 409),
    SharingTooManySessions = ("SHARING.TOO_MANY_SESSIONS", 409),
    SavesProfileNotFound = ("SAVES.PROFILE_NOT_FOUND", 404),
    SavesLeaseHeld = ("SAVES.LEASE_HELD", 409),
    SavesLeaseLost = ("SAVES.LEASE_LOST", 409),
    SavesLeaseExpired = ("SAVES.LEASE_EXPIRED", 409),
    SavesRevisionConflict = ("SAVES.REVISION_CONFLICT", 412),
    SavesIdempotencyKeyReused = ("SAVES.IDEMPOTENCY_KEY_REUSED", 422),
    SavesRevisionNotFound = ("SAVES.REVISION_NOT_FOUND", 404),
    SavesBundleInvalid = ("SAVES.BUNDLE_INVALID", 422),
    SavesDigestMismatch = ("SAVES.DIGEST_MISMATCH", 400),
    SavesGateRefused = ("SAVES.GATE_REFUSED", 422),
    SavesSummaryUnreadable = ("SAVES.SUMMARY_UNREADABLE", 422),
    SavesHardcoreNeedsFreshCareer = ("SAVES.HARDCORE_NEEDS_FRESH_CAREER", 409),
    SavesNotEligible = ("SAVES.NOT_ELIGIBLE", 403),
    SavesRestoreOutsideRetention = ("SAVES.RESTORE_OUTSIDE_RETENTION", 403),
    SavesRestoreBehindReset = ("SAVES.RESTORE_BEHIND_RESET", 403),
    SavesRestoreFlagged = ("SAVES.RESTORE_FLAGGED", 409),
    SavesInvalidArgument = ("SAVES.INVALID_ARGUMENT", 400),
    SavesRateLimited = ("SAVES.RATE_LIMITED", 429),
    SavesUnavailable = ("SAVES.UNAVAILABLE", 503),
    CreationsQuotaExceeded = ("CREATIONS.QUOTA_EXCEEDED", 409),
    SocialLimitReached = ("SOCIAL.LIMIT_REACHED", 409),
    VoiceNotShared = ("VOICE.NOT_SHARED", 404),
    VoiceRateLimited = ("VOICE.RATE_LIMITED", 429),
    PortalSessionRequired = ("PORTAL.SESSION_REQUIRED", 401),
    PortalVipRequired = ("PORTAL.VIP_REQUIRED", 403),
    PortalNotFound = ("PORTAL.NOT_FOUND", 404),
    PortalConflict = ("PORTAL.CONFLICT", 409),
    PortalModeMismatch = ("PORTAL.MODE_MISMATCH", 409),
    BoardsNotFound = ("BOARDS.NOT_FOUND", 404),
    PlayersNotFound = ("PLAYERS.NOT_FOUND", 404),
    ClubsNotFound = ("CLUBS.NOT_FOUND", 404),
    PoolFull = ("POOL.FULL", 503),
    ServiceUnavailable = ("SERVICE.UNAVAILABLE", 503),
    ServiceMaintenance = ("SERVICE.MAINTENANCE", 503),
    ServiceBudgetClosed = ("SERVICE.BUDGET_CLOSED", 503),
    ServiceOnlineClosed = ("SERVICE.ONLINE_CLOSED", 503),
}

impl ErrorCode {
    /// Parses a wire string.
    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.code() == code)
    }

    /// The envelope for this code.
    pub fn body(self, message: impl Into<String>, details: Option<serde_json::Value>) -> ErrorBody {
        ErrorBody {
            code: self.code().to_owned(),
            message: message.into(),
            details,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_are_unique_well_formed_and_parse() {
        let mut seen = HashSet::new();
        for c in ErrorCode::ALL {
            assert!(seen.insert(c.code()), "duplicate {}", c.code());
            let (domain, name) = c.code().split_once('.').expect("DOMAIN.CODE");
            assert!(
                domain.bytes().all(|b| b.is_ascii_uppercase())
                    && name.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
            );
            assert!((400..600).contains(&c.status()));
            assert_eq!(ErrorCode::parse(c.code()), Some(*c));
        }
    }

    #[test]
    fn the_envelope_is_flat() {
        let body = ErrorCode::SavesRevisionConflict.body(
            "head moved",
            Some(serde_json::json!({"expected": 41, "head": 42})),
        );
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"code": "SAVES.REVISION_CONFLICT", "message": "head moved", "details": {"expected": 41, "head": 42}})
        );
        let bare = serde_json::to_string(&ErrorCode::PoolFull.body("full", None)).unwrap();
        assert_eq!(bare, r#"{"code":"POOL.FULL","message":"full"}"#);
    }
}
