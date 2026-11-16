//! The platform session on core (ADR 0014 §1, CORE_API.md §1): a launcher access token
//! (`Authorization: Bearer`, `aud = launcher`) or the web's `__Host-fi_session` cookie. Never a
//! game access ticket: [`crate::guard::ticket_guard`] answers `AUTH.TOKEN_INVALID` to any bearer
//! that is not a launcher access token before a handler runs.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use fi_db::moderation::{Actor, Role};
use fi_protocol::ErrorCode;
use fi_surfaces::errors::ApiError;
use fi_surfaces::extract::{LauncherAuth, PortalAuth};
use fi_surfaces::state::AppState;
use serde_json::json;
use uuid::Uuid;

/// A signed-in caller of core.
#[derive(Clone, Debug)]
pub struct CoreSession {
    /// The account.
    pub account_id: Uuid,
    /// Its role at this request.
    pub role: Role,
    /// Came with the web's cookie (else a launcher bearer).
    pub web: bool,
    /// The caller's `X-Request-Id`, kept in audit rows.
    pub request_id: Option<String>,
}

impl CoreSession {
    /// `AUTH.ROLE_REQUIRED {role}` unless the caller holds at least `role`.
    pub fn require(&self, role: Role) -> Result<(), ApiError> {
        if self.role >= role {
            Ok(())
        } else {
            Err(
                ApiError::new(ErrorCode::AuthRoleRequired, "this route needs a staff role")
                    .with_details(json!({"role": role.as_str()})),
            )
        }
    }

    /// The audit actor (moderator or admin).
    pub fn actor(&self) -> Actor {
        Actor {
            account_id: self.account_id,
            role: self.role,
            request_id: self.request_id.clone(),
        }
    }
}

fn request_id(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(fi_runtime::http::REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| (1..=128).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_graphic()))
        .map(str::to_owned)
}

impl FromRequestParts<AppState> for CoreSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if let Some(session) = parts.extensions.get::<CoreSession>() {
            return Ok(session.clone());
        }
        let request_id = request_id(parts);
        if parts.headers.contains_key(AUTHORIZATION) {
            let auth = LauncherAuth::from_request_parts(parts, state).await?;
            return Ok(CoreSession {
                account_id: auth.account_id,
                role: Role::parse(&auth.account.role).unwrap_or(Role::Player),
                web: false,
                request_id,
            });
        }
        if !parts
            .extensions
            .get::<std::sync::Arc<crate::Core>>()
            .is_some_and(|core| core.config.web_enabled)
        {
            return Err(ApiError::new(
                ErrorCode::PortalSessionRequired,
                "web sessions are unavailable",
            ));
        }
        let auth = PortalAuth::from_request_parts(parts, state).await?;
        Ok(CoreSession {
            account_id: auth.account_id,
            role: Role::parse(&auth.account.role).unwrap_or(Role::Player),
            web: true,
            request_id,
        })
    }
}

/// The least role each staff path needs; handlers may still ask for more.
pub fn required_role(path: &str) -> Option<Role> {
    let rest = path.strip_prefix("/core/v1/admin/")?;
    let section = rest.split('/').next().unwrap_or_default();
    let admin_only = matches!(section, "maintenance" | "capacity" | "notices" | "bans")
        || rest.ends_with("/role")
        || rest.ends_with("/erase")
        || rest.ends_with("/seed-revoke");
    Some(if admin_only {
        Role::Admin
    } else {
        Role::Moderator
    })
}

/// Refuses a staff path before its handler unless the caller holds the role it needs.
pub async fn require_role(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let Some(role) = required_role(request.uri().path()) else {
        return next.run(request).await;
    };
    let (mut parts, body) = request.into_parts();
    let session = match CoreSession::from_request_parts(&mut parts, &state).await {
        Ok(session) => session,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = session.require(role) {
        return e.into_response();
    }
    parts.extensions.insert(session);
    next.run(Request::from_parts(parts, body)).await
}

/// An optional session for public routes: no credential is anonymous, a cookie that no longer
/// opens a session is anonymous too (a stale cookie must not hide a public page), a bearer that
/// fails is an error (the launcher refreshes).
pub struct MaybeSession(pub Option<CoreSession>);

impl FromRequestParts<AppState> for MaybeSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if parts.headers.contains_key(AUTHORIZATION) {
            return CoreSession::from_request_parts(parts, state)
                .await
                .map(|s| MaybeSession(Some(s)));
        }
        if fi_surfaces::extract::session_cookie(&parts.headers).is_none() {
            return Ok(MaybeSession(None));
        }
        match CoreSession::from_request_parts(parts, state).await {
            Ok(s) => Ok(MaybeSession(Some(s))),
            Err(e) if e.code == ErrorCode::PortalSessionRequired => Ok(MaybeSession(None)),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod role_tests {
    use super::{Role, required_role};

    /// CR-S52: the seed revoke bans, so it is an admin's, as bans are; the profile section
    /// otherwise stays a moderator's.
    #[test]
    fn the_seed_revoke_is_admin_only() {
        assert_eq!(
            required_role(
                "/core/v1/admin/profiles/0192f0e0-0000-7000-8000-000000000000/seed-revoke"
            ),
            Some(Role::Admin)
        );
        assert_eq!(required_role("/core/v1/admin/bans"), Some(Role::Admin));
        assert_eq!(
            required_role("/core/v1/admin/assessments"),
            Some(Role::Moderator)
        );
        assert_eq!(required_role("/core/v1/status"), None);
    }

    /// CR-S62: a kick bans nothing, so moderators may (the handler refuses staff targets).
    #[test]
    fn the_kick_is_a_moderators() {
        assert_eq!(required_role("/core/v1/admin/kicks"), Some(Role::Moderator));
    }
}
