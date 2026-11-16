//! Every refusal as the contract's envelope (`{code, message, details}`, ACCOUNTS.md §1, §9): the
//! HTTP status comes from the code (`fi_protocol::ErrorCode`), 429 and 503 carry `Retry-After`.
//! Messages are English for logs and never contain a secret, a token, an e-mail or an IP.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fi_db::identity::IdentityError;
use fi_protocol::ErrorCode;
use serde_json::{Value, json};
use std::borrow::Cow;

/// A refusal.
#[derive(Debug)]
pub struct ApiError {
    /// The stable code.
    pub code: ErrorCode,
    /// English, for logs.
    pub message: Cow<'static, str>,
    /// Structured context.
    pub details: Option<Value>,
    /// `Retry-After` seconds (429, 503).
    pub retry_after: Option<u64>,
}

impl ApiError {
    /// A refusal with `code`.
    pub fn new(code: ErrorCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
            retry_after: None,
        }
    }

    /// `REQUEST.INVALID` naming the field.
    pub fn invalid(field: impl Into<String>, message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(ErrorCode::RequestInvalid, message).with_details(json!({"field": field.into()}))
    }

    /// With details.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// CR-S98 (CAREERS.md §6.5): a `SESSIONS.PROFILE_IN_USE` names the game of the career in use
    /// (`details.game`, beside its other details); any other refusal is unchanged.
    pub fn in_game(mut self, game: &str) -> Self {
        if self.code == ErrorCode::SessionsProfileInUse {
            let mut details = match self.details.take() {
                Some(Value::Object(m)) => m,
                _ => serde_json::Map::new(),
            };
            details.insert("game".into(), Value::String(game.to_owned()));
            self.details = Some(Value::Object(details));
        }
        self
    }

    /// With `Retry-After`.
    pub fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs.max(1));
        self
    }

    /// `SERVICE.UNAVAILABLE`, retry soon.
    pub fn unavailable() -> Self {
        Self::new(ErrorCode::ServiceUnavailable, "service unavailable, retry").retry_after(1)
    }

    /// A failure to store data the request carried (the game plane's stored methods,
    /// GAME_PLATFORM.md §3a). What the database refuses for the data itself never succeeds on a
    /// retry, so it is the request's 4xx, not a 503 the game would repeat forever: a size limit (a
    /// CHECK constraint named `*_size`, or `54000` program_limit_exceeded) is 413
    /// `REQUEST.TOO_LARGE`; another CHECK (`23514`) or text Postgres cannot hold (`22P05`
    /// untranslatable_character, `22021` character_not_in_repertoire) is 400
    /// `REQUEST.INVALID {field: "body"}`. Anything else maps as `From<IdentityError>`.
    pub fn from_stored(e: IdentityError) -> Self {
        let IdentityError::Db(sqlx::Error::Database(db)) = &e else {
            return e.into();
        };
        let size = || ApiError::new(ErrorCode::RequestTooLarge, "the stored data is too large");
        match db.code().as_deref() {
            Some("54000") => size(),
            Some("23514") if db.constraint().is_some_and(|c| c.ends_with("_size")) => size(),
            Some("23514" | "22P05" | "22021") => {
                ApiError::invalid("body", "the database refuses this data")
            }
            _ => e.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.code.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let refusal = fi_runtime::http::Refusal {
            code: self.code.code(),
            field: self
                .details
                .as_ref()
                .and_then(|d| d["field"].as_str())
                .map(|f| f.chars().take(64).collect()),
        };
        let body = self.code.body(self.message, self.details);
        let mut response = (status, Json(body)).into_response();
        response.extensions_mut().insert(refusal);
        let retry = match (self.retry_after, status.as_u16()) {
            (Some(s), _) => Some(s),
            (None, 429 | 503) => Some(1),
            _ => None,
        };
        if let Some(secs) = retry
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, v);
        }
        response
    }
}

impl From<IdentityError> for ApiError {
    fn from(e: IdentityError) -> Self {
        match e {
            IdentityError::Db(err) => {
                // The error names the failing statement, never a bound value.
                tracing::error!(error = %err, "database error");
                ApiError::unavailable()
            }
            IdentityError::Busy => ApiError::unavailable(),
            IdentityError::NameUnavailable => ApiError::new(
                ErrorCode::AccountsNameUnavailable,
                "that name is taken or reserved",
            ),
            IdentityError::ProfileLimit => ApiError::new(
                ErrorCode::AccountsProfileLimit,
                "the account already has a live profile of this mode",
            ),
            IdentityError::Banned { review } => banned(review.as_deref()),
            IdentityError::AlreadyLinked => ApiError::new(
                ErrorCode::AccountsAlreadyLinked,
                "the identity or the account is already linked",
            ),
            IdentityError::TooManyDevices => {
                ApiError::new(ErrorCode::SharingTooManyDevices, "too many devices")
            }
            IdentityError::TooManyRecentDevices => ApiError::new(
                ErrorCode::SharingTooManyRecentDevices,
                "too many new devices recently",
            ),
            IdentityError::NotFound => ApiError::new(ErrorCode::AccountsNotFound, "not found"),
            IdentityError::NotPendingDeletion => ApiError::new(
                ErrorCode::AccountsNotPendingDeletion,
                "the account is not pending deletion",
            ),
            IdentityError::IdempotencyKeyReused => ApiError::new(
                ErrorCode::RequestIdempotencyKeyReused,
                "this Idempotency-Key was used for another request",
            ),
            IdentityError::Event(reason) => {
                tracing::error!(%reason, "event did not validate");
                ApiError::unavailable()
            }
        }
    }
}

impl From<fi_db::purge::ModeChangeError> for ApiError {
    fn from(e: fi_db::purge::ModeChangeError) -> Self {
        use fi_db::purge::ModeChangeError as M;
        match e {
            M::NotFound => ApiError::new(ErrorCode::AccountsNotFound, "no such profile"),
            M::DeletionPending => ApiError::new(
                ErrorCode::AccountsDeletionPending,
                "the account or the profile is pending deletion",
            ),
            M::Forbidden => ApiError::new(
                ErrorCode::AccountsModeChangeForbidden,
                "a profile can only go from hardcore to noob",
            ),
            M::Refused { reason } => ApiError::new(
                ErrorCode::AccountsModeChangeForbidden,
                "the profile's mode cannot change now",
            )
            .with_details(json!({ "reason": reason })),
            M::InUse => ApiError::new(
                ErrorCode::SessionsProfileInUse,
                "the profile has a play session or a save lease: end the session first",
            ),
            M::TicketLive { until } => {
                let wait = (until.millis() - fi_domain::UnixMs::now_system().millis())
                    .div_euclid(1_000)
                    .clamp(1, 600) as u64;
                ApiError::new(
                    ErrorCode::SessionsProfileInUse,
                    "the profile's last game ticket is still valid: try again when it expires",
                )
                .with_details(json!({ "ticketExpiresAt": wire(until) }))
                .retry_after(wait)
            }
            M::Db(e) => e.into(),
        }
    }
}

impl From<fi_db::erasure::ProfileDeletionError> for ApiError {
    fn from(e: fi_db::erasure::ProfileDeletionError) -> Self {
        use fi_db::erasure::ProfileDeletionError as D;
        match e {
            D::NotFound => ApiError::new(ErrorCode::AccountsNotFound, "no such profile"),
            D::DeletionPending => ApiError::new(
                ErrorCode::AccountsDeletionPending,
                "the account or the profile is pending deletion",
            ),
            D::Banned { review } => banned(review.as_deref()),
            D::RateLimited { retry_after_secs } => ApiError::new(
                ErrorCode::RequestRateLimited,
                "at most 3 profiles are deleted per hour: try again later",
            )
            .retry_after(retry_after_secs),
            D::InUse => fi_db::purge::ModeChangeError::InUse.into(),
            D::TicketLive { until } => fi_db::purge::ModeChangeError::TicketLive { until }.into(),
            D::Career { game, error } => ApiError::from(*error).in_game(&game),
            D::Db(e) => e.into(),
        }
    }
}

impl From<fi_db::rename::RenameError> for ApiError {
    fn from(e: fi_db::rename::RenameError) -> Self {
        use fi_db::rename::RenameError as R;
        match e {
            R::NotFound => ApiError::new(ErrorCode::AccountsNotFound, "no such profile"),
            R::DeletionPending => ApiError::new(
                ErrorCode::AccountsDeletionPending,
                "the account or the profile is pending deletion",
            ),
            R::NotAllowed => ApiError::new(
                ErrorCode::AccountsRenameNotAllowed,
                "this profile has no free rename",
            ),
            R::NameUnavailable => IdentityError::NameUnavailable.into(),
            R::InUse { game } => ApiError::new(
                ErrorCode::SessionsProfileInUse,
                "the profile has a play session or a save lease: end the session first",
            )
            .in_game(&game),
            R::TicketLive { until } => {
                let wait = (until.millis() - fi_domain::UnixMs::now_system().millis())
                    .div_euclid(1_000)
                    .clamp(1, 600) as u64;
                ApiError::new(
                    ErrorCode::SessionsProfileInUse,
                    "the profile's last game ticket is still valid: try again when it expires",
                )
                .with_details(json!({ "ticketExpiresAt": wire(until) }))
                .retry_after(wait)
            }
            R::Db(e) => e.into(),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        IdentityError::from(e).into()
    }
}

/// Whole seconds from now to `until`, rounded up, 1 to 3600 (`Retry-After` of a kick's cooldown,
/// at most 60 minutes).
fn seconds_until(until: fi_domain::UnixMs) -> u64 {
    let ms = until.millis() - fi_domain::UnixMs::now_system().millis();
    (ms + 999).div_euclid(1_000).clamp(1, 3_600) as u64
}

fn wire(t: fi_domain::UnixMs) -> String {
    fi_protocol::instant::format_unix_ms(t.millis()).unwrap_or_default()
}

/// Save refusals as SAVES_API.md §6 (the code's status comes from `fi_protocol::ErrorCode`).
impl From<fi_saves::SaveError> for ApiError {
    fn from(e: fi_saves::SaveError) -> Self {
        use fi_saves::SaveError as S;
        let code = ErrorCode::parse(e.code()).unwrap_or(ErrorCode::SavesUnavailable);
        let details = match &e {
            S::LeaseHeld {
                device_label,
                expires_at,
                ..
            } => Some(json!({"deviceLabel": device_label, "expiresAt": wire(*expires_at)})),
            S::LeaseLost { current } => Some(json!({"current": current.map(|t| t.get())})),
            S::LeaseExpired { expired_at } => Some(json!({"expiredAt": wire(*expired_at)})),
            S::RevisionConflict { expected, head } => {
                Some(json!({"expected": expected.get(), "head": head.get()}))
            }
            S::BundleInvalid(reason) => Some(json!({"reason": reason.to_string()})),
            S::GateRefused {
                reason,
                parked,
                rule,
                score,
                reasons,
            } => Some(json!({
                "reason": reason,
                "parkedId": parked.to_string(),
                "rule": rule,
                "score": score,
                "reasons": reasons,
            })),
            S::SummaryUnreadable { parked } => Some(json!({"parkedId": parked.to_string()})),
            S::RestoreBehindReset { reset } => Some(json!({"reset": reset.get()})),
            S::RestoreFlagged { revision } => Some(json!({"revision": revision.get()})),
            S::NotEligible { tier, mode } => Some(json!({"tier": tier, "mode": mode})),
            S::TooManyParked { parked } => Some(json!({"reason": "parked_full", "parked": parked})),
            _ => None,
        };
        let message: Cow<'static, str> = match &e {
            // Storage messages may name internal endpoints: logged, never sent.
            S::Unavailable(detail) => {
                tracing::warn!(%detail, "save storage unavailable");
                "save storage unavailable, retry with the same Idempotency-Key".into()
            }
            other => other.to_string().into(),
        };
        let mut out = ApiError::new(code, message);
        out.details = details;
        if matches!(e, S::Unavailable(_)) {
            out = out.retry_after(1);
        }
        // SAVES_API.md §4.6: a slot frees only when a parked bundle expires, but the launcher
        // waits `Retry-After` uncapped while it shows "Uploading" at exit: keep it short.
        if matches!(e, S::TooManyParked { .. }) {
            out = out.retry_after(60);
        }
        out
    }
}

/// Admission refusals (SAVES_API.md §3, ACCOUNTS.md §7).
impl From<fi_db::admission::AdmissionError> for ApiError {
    fn from(e: fi_db::admission::AdmissionError) -> Self {
        use fi_db::admission::AdmissionError as A;
        match e {
            A::Frozen { until } => ApiError::new(
                ErrorCode::AccountsFrozen,
                "online access temporarily restricted by human moderation",
            )
            .with_details(json!({"until":wire(until)})),
            A::Kicked { until } => ApiError::new(
                ErrorCode::SessionsKicked,
                "removed from online play by a moderator; try again later",
            )
            .with_details(json!({"until": wire(until)}))
            .retry_after(seconds_until(until)),
            A::InUseElsewhere {
                device_label,
                since,
            } => ApiError::new(
                ErrorCode::SessionsInUseElsewhere,
                "this account is playing on another device",
            )
            .with_details(json!({"deviceLabel": device_label, "since": wire(since)})),
            A::PoolFull => {
                ApiError::new(ErrorCode::PoolFull, "the pool's queue is full").retry_after(30)
            }
            A::PoolClosed => {
                ApiError::new(ErrorCode::PoolFull, "the pool does not accept sessions now")
                    .retry_after(30)
            }
            A::RoleRequired => ApiError::new(
                ErrorCode::AuthRoleRequired,
                "a dedicated lobby needs the admin role",
            )
            .with_details(json!({"role": "admin"})),
            A::ModdedHardcore => ApiError::new(
                ErrorCode::RequestInvalid,
                "a Hardcore profile runs no multiplayer mod",
            )
            .with_details(json!({"field": "modded", "reason": "hardcore"})),
            A::KeyReused => ApiError::new(
                ErrorCode::RequestIdempotencyKeyReused,
                "this session id belongs to another session",
            ),
            A::NotFound => ApiError::new(ErrorCode::AccountsNotFound, "no such play session"),
            A::Ended { reason } => ApiError::new(ErrorCode::SessionsEnded, "the session ended")
                .with_details(json!({"reason": reason})),
            A::Queued => ApiError::new(
                ErrorCode::SavesInvalidArgument,
                "the session is queued: poll GET /launcher/v1/sessions/{id}",
            ),
            A::NotEligible { tier, mode } => ApiError::new(
                ErrorCode::SavesNotEligible,
                "this profile keeps no cloud save on this server: use /gate/v1",
            )
            .with_details(json!({"tier": tier.as_str(), "mode": mode})),
            A::Save(e) => e.into(),
            A::Busy => ApiError::unavailable(),
            A::Db(err) => {
                tracing::error!(error = %err, "database error");
                ApiError::unavailable()
            }
            A::Event(reason) => {
                tracing::error!(%reason, "admission invariant");
                ApiError::unavailable()
            }
        }
    }
}

/// Progression-fact refusals (GAME_PLATFORM.md §2.2).
impl From<fi_db::game::FactError> for ApiError {
    fn from(e: fi_db::game::FactError) -> Self {
        use fi_db::game::FactError as F;
        match e {
            F::Kind => ApiError::invalid("kind", "unknown progression kind"),
            F::Data(field, reason) => ApiError::invalid(field, reason),
            F::Conflict => ApiError::new(
                ErrorCode::RequestIdempotencyKeyReused,
                "this request id names another fact",
            ),
            F::Db(e) => e.into(),
        }
    }
}

/// 403 `ACCOUNTS.BANNED` (ACCOUNTS.md §10), with `details: {review}` when the ban is a block
/// pending review the player is told about (`account.ban_review`; CR-S52's `seed_from_profile`:
/// a Hardcore profile seeded from another profile whose progression was found altered). The ban's
/// free-text reason is never sent.
pub fn banned(review: Option<&str>) -> ApiError {
    let e = ApiError::new(ErrorCode::AccountsBanned, "account banned");
    match review {
        Some(review) => e.with_details(serde_json::json!({ "review": review })),
        None => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn a_refusal_tells_the_access_log_its_code_and_field() {
        let r = ApiError::invalid("xuids", "1 to 12 distinct XUIDs").into_response();
        assert_eq!(
            r.extensions().get::<fi_runtime::http::Refusal>(),
            Some(&fi_runtime::http::Refusal {
                code: "REQUEST.INVALID",
                field: Some("xuids".into()),
            })
        );
        let r = ApiError::unavailable().into_response();
        let refusal = r.extensions().get::<fi_runtime::http::Refusal>().unwrap();
        assert_eq!(
            (refusal.code, refusal.field.as_deref()),
            ("SERVICE.UNAVAILABLE", None)
        );
    }

    async fn body(e: ApiError) -> (StatusCode, Option<String>, Value) {
        let r = e.into_response();
        let status = r.status();
        let retry = r
            .headers()
            .get(header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = to_bytes(r.into_body(), 4096).await.unwrap();
        (status, retry, serde_json::from_slice(&bytes).unwrap())
    }

    /// A database refusal as Postgres reports it: a SQLSTATE and, for constraints, a name.
    #[derive(Debug)]
    struct Refused(&'static str, Option<&'static str>);

    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "refused {}", self.0)
        }
    }

    impl std::error::Error for Refused {}

    impl sqlx::error::DatabaseError for Refused {
        fn message(&self) -> &str {
            "refused"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(self.0.into())
        }
        fn constraint(&self) -> Option<&str> {
            self.1
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn stored(code: &'static str, constraint: Option<&'static str>) -> ApiError {
        ApiError::from_stored(IdentityError::Db(sqlx::Error::Database(Box::new(Refused(
            code, constraint,
        )))))
    }

    /// What the database refuses for the data itself is a 4xx (the game would repeat a 503 with
    /// the same data forever); a transient failure stays 503 with Retry-After.
    #[tokio::test]
    async fn stored_data_refusals_are_permanent() {
        for (code, constraint) in [
            ("23514", Some("game_profile_size")),
            ("23514", Some("game_profile_table_size")),
            ("54000", None),
        ] {
            let (s, retry, b) = body(stored(code, constraint)).await;
            assert_eq!(
                (s.as_u16(), retry, b["code"].as_str()),
                (413, None, Some("REQUEST.TOO_LARGE")),
                "{code} {constraint:?}"
            );
        }
        for (code, constraint) in [
            ("23514", Some("game_profile_table_name_check")),
            ("22P05", None),
            ("22021", None),
        ] {
            let (s, retry, b) = body(stored(code, constraint)).await;
            assert_eq!(
                (s.as_u16(), retry, b["code"].as_str()),
                (400, None, Some("REQUEST.INVALID")),
                "{code}"
            );
            assert_eq!(b["details"]["field"], "body");
        }
        for e in [
            stored("08006", None),
            stored("23505", Some("x")),
            ApiError::from_stored(IdentityError::Busy),
        ] {
            let (s, retry, _) = body(e).await;
            assert_eq!((s.as_u16(), retry.as_deref()), (503, Some("1")));
        }
        let (s, _, _) = body(ApiError::from_stored(IdentityError::NotFound)).await;
        assert_eq!(s.as_u16(), 404, "other refusals map as before");
    }

    #[tokio::test]
    async fn the_envelope_status_and_retry_after() {
        let (s, retry, b) = body(ApiError::invalid("device.platform", "unknown platform")).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(retry, None);
        assert_eq!(
            b,
            json!({"code": "REQUEST.INVALID", "message": "unknown platform", "details": {"field": "device.platform"}})
        );
        let (s, retry, b) =
            body(ApiError::new(ErrorCode::RequestRateLimited, "slow down").retry_after(42)).await;
        assert_eq!((s.as_u16(), retry.as_deref()), (429, Some("42")));
        assert_eq!(b["code"], "REQUEST.RATE_LIMITED");
        let (s, retry, _) = body(ApiError::from(IdentityError::Busy)).await;
        assert_eq!((s.as_u16(), retry.as_deref()), (503, Some("1")));
        let (s, _, b) = body(ApiError::from(IdentityError::AlreadyLinked)).await;
        assert_eq!(
            (s.as_u16(), b["code"].as_str()),
            (409, Some("ACCOUNTS.ALREADY_LINKED"))
        );
        let (s, _, b) = body(ApiError::from(IdentityError::TooManyRecentDevices)).await;
        assert_eq!(
            (s.as_u16(), b["code"].as_str()),
            (409, Some("SHARING.TOO_MANY_RECENT_DEVICES"))
        );
    }
}
