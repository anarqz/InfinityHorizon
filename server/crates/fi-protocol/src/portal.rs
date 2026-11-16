//! Portal API bodies, `/portal/v1` (PORTAL_API.md §1, §3): sessions and account, the launcher's
//! authorization pages, deletion. The portal's server (the BFF) is the only caller; it sends the
//! visitor's Identity Platform ID token where a route needs a fresh sign-in, and its portal session
//! token as `Authorization: Bearer` everywhere else. Mirror: `packages/contracts/src/portal.ts`.

use crate::accounts::{Access, Deletion, Notice, Profile};
use serde::{Deserialize, Serialize};

/// `GET /portal/v1/health` → 200 when the surface can serve.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalHealth {
    /// `ok`.
    pub status: String,
}

/// `POST /portal/v1/sessions`: the Identity Platform exchange (ACCOUNTS.md §4.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalSignIn {
    /// A force-refreshed Identity Platform ID token (at most 5 minutes old).
    pub id_token: String,
}

/// The signed-in visitor (`GET /portal/v1/me`, and in every session answer).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewer {
    /// The account.
    pub account_id: String,
    /// `player`, `moderator`.
    pub role: String,
    /// Its profiles (ACCOUNTS.md §2).
    pub profiles: Vec<Profile>,
    /// Supporter access (ACCOUNTS.md §7).
    pub access: Access,
    /// Notices (ACCOUNTS.md §5).
    pub notices: Vec<Notice>,
    /// Pending deletion, `null` when none.
    pub deletion: Option<Deletion>,
}

/// `POST /portal/v1/sessions` → 201: the portal's variant of the exchange outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PortalSignInResult {
    /// The identity maps to an account: a portal session token (ACCOUNTS.md §4.2) opens.
    SignedIn {
        /// `fi_ps_…`, opaque; the BFF keeps it in its `__Host-fi_session` cookie.
        token: String,
        /// Sliding expiry (30 days, at most 90 after the sign-in).
        expires_at: String,
        /// Who signed in.
        viewer: Box<Viewer>,
    },
    /// Unknown identity: create an account or link a Beta account with the ticket.
    NeedsAccount {
        /// `fi_it_…`, 15 minutes, single use.
        identity_ticket: String,
        /// Its expiry.
        identity_ticket_expires_at: String,
        /// The identity's verified e-mail, to show which identity this is.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        email: Option<String>,
    },
}

/// `{token, expiresAt, viewer}`: `POST /portal/v1/sessions/launcher`, `/accounts`, `/legacy/link`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalSessionIssued {
    /// `fi_ps_…`.
    pub token: String,
    /// Sliding expiry.
    pub expires_at: String,
    /// Who signed in.
    pub viewer: Viewer,
}

/// `POST /portal/v1/sessions/launcher`: redeem a launcher portal code (ACCOUNTS.md §4.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalCodeRedeem {
    /// From `POST /launcher/v1/portal-codes`, single use, 60 s.
    pub code: String,
}

/// `POST /portal/v1/accounts`: an account without profiles (the launcher adds them).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalAccountCreate {
    /// From a `needs_account` [`PortalSignInResult`].
    pub identity_ticket: String,
    /// BCP 47 tag (`en`, `pt-BR`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
}

/// `POST /portal/v1/legacy/link`: an identity ticket plus a Beta account's gamertag and site key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalLegacyLink {
    /// From a `needs_account` [`PortalSignInResult`].
    pub identity_ticket: String,
    /// Any gamertag of the Beta account.
    pub gamertag: String,
    /// `XXXX-XXXX-XXXX`.
    pub site_key: String,
}

/// `POST /portal/v1/me/deletion`: typed confirmation plus a recent sign-in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalDeletionRequest {
    /// One of the account's gamertags.
    pub confirm_gamertag: String,
    /// A fresh ID token (`auth_time` within 5 minutes).
    pub id_token: String,
}

/// One token family of the account (`GET /portal/v1/me/sessions`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    /// The family.
    pub id: String,
    /// The family of this request.
    pub current: bool,
    /// `portal`, `launcher`.
    pub client: String,
    /// Device label, or the browser's.
    pub label: String,
    /// When signed in.
    pub created_at: String,
    /// Last use.
    pub last_seen_at: String,
    /// Country of the last use, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip_country: Option<String>,
}

/// `GET /portal/v1/me/sessions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionList {
    /// Newest first.
    pub items: Vec<SessionSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn viewer() -> Viewer {
        Viewer {
            account_id: "0192f0c4-0000-7000-8000-000000000002".into(),
            role: "player".into(),
            profiles: vec![],
            access: Access {
                tier: "free".into(),
                status: "none".into(),
                access_until: None,
                plan: None,
                provider: None,
                auto_renew: false,
                past_due: false,
            },
            notices: vec![],
            deletion: None,
        }
    }

    #[test]
    fn sign_in_results_use_the_portal_spelling() {
        let signed = PortalSignInResult::SignedIn {
            token: "fi_ps_x".into(),
            expires_at: "2026-11-16T14:00:00.000000Z".into(),
            viewer: Box::new(viewer()),
        };
        let wire = serde_json::to_value(&signed).unwrap();
        assert_eq!(wire["status"], "signed_in");
        assert_eq!(wire["expiresAt"], "2026-11-16T14:00:00.000000Z");
        assert_eq!(
            wire["viewer"]["deletion"],
            serde_json::Value::Null,
            "null, not absent"
        );
        assert_eq!(wire["viewer"]["access"]["autoRenew"], false);
        assert_eq!(
            serde_json::from_value::<PortalSignInResult>(wire).unwrap(),
            signed
        );
        let needs = json!({"status": "needs_account", "identityTicket": "fi_it_y",
                           "identityTicketExpiresAt": "2026-10-17T14:15:00.000000Z"});
        assert!(matches!(
            serde_json::from_value::<PortalSignInResult>(needs).unwrap(),
            PortalSignInResult::NeedsAccount { email: None, .. }
        ));
    }

    #[test]
    fn request_bodies_refuse_unknown_fields() {
        assert!(serde_json::from_value::<PortalSignIn>(json!({"idToken": "t", "x": 1})).is_err());
        let account: PortalAccountCreate =
            serde_json::from_value(json!({"identityTicket": "fi_it_z"})).unwrap();
        assert_eq!(account.locale, None);
    }
}
