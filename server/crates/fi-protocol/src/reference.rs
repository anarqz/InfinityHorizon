//! Frozen pre-OD-74 wire shapes, exclusively for the deprecated Rust launcher and its fake server.
//! Not part of the active HTTP contract; production services must use accounts/portal/routes.
//! Kept here so the frozen reference compiles without restoring removed production endpoints.

/// Historical launcher account DTOs.
pub mod accounts {
    pub use crate::accounts::*;
    use serde::{Deserialize, Serialize};
    /// `POST /launcher/v1/auth/token`: the Identity Platform sign-in exchange (ACCOUNTS.md §4.1, §5).
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(
        tag = "grantType",
        rename_all = "snake_case",
        rename_all_fields = "camelCase",
        deny_unknown_fields
    )]
    pub enum TokenGrant {
        /// Native sign-in (OD-43): a fresh Identity Platform ID token the launcher minted itself with
        /// `accounts:signInWithPassword`, verified as in ACCOUNTS.md §4.1.
        IdToken {
            /// The ID token (JWT), at most 5 minutes after its `iat`.
            id_token: String,
            /// This launcher install.
            device: Device,
        },
        /// Loopback redirect + PKCE (RFC 8252): the code from `<portal>/auth/launcher/sign-in`.
        AuthorizationCode {
            /// Single use, 60 s.
            code: String,
            /// The PKCE verifier (43-128 characters); its S256 challenge was sent with the code.
            code_verifier: String,
            /// Exactly `http://127.0.0.1:<port>/callback`, as sent to the portal.
            redirect_uri: String,
            /// This launcher install.
            device: Device,
        },
        /// Device code, approved by the player on `<portal>/link`.
        DeviceCode {
            /// From `POST /launcher/v1/auth/device`.
            device_code: String,
            /// This launcher install.
            device: Device,
        },
    }

    /// `POST /launcher/v1/auth/device`.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct DeviceAuthorizationRequest {
        /// This launcher install (its label is shown on the approval page).
        pub device: Device,
    }

    /// `POST /launcher/v1/auth/device` → 201.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DeviceAuthorization {
        /// Polled with [`TokenGrant::DeviceCode`].
        pub device_code: String,
        /// `XXXX-XXXX`, typed by the player on the portal.
        pub user_code: String,
        /// `<portal>/link`.
        pub verification_uri: String,
        /// Seconds until both codes expire (600).
        pub expires_in: u32,
        /// Seconds between polls (5).
        pub interval: u32,
    }
}

/// Historical launcher authorization DTOs.
pub mod portal {
    pub use crate::portal::*;
    use serde::{Deserialize, Serialize};
    /// `POST /portal/v1/auth/launcher-codes`: the launcher's authorization page consented (RFC 8252).
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct LauncherCodeRequest {
        /// A fresh ID token of the visitor.
        pub id_token: String,
        /// Exactly `http://127.0.0.1:<port>/callback`.
        pub redirect_uri: String,
        /// `base64url(SHA-256(codeVerifier))`, 43 characters.
        pub code_challenge: String,
        /// `S256`.
        pub code_challenge_method: String,
        /// The PC name the consent page showed.
        pub device_label: String,
    }

    /// `POST /portal/v1/auth/launcher-codes` → 201.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct LauncherCodeIssued {
        /// 32 base32 characters, single use; the browser carries it to the loopback redirect.
        pub code: String,
        /// 60 s after issue.
        pub expires_at: String,
    }

    /// `POST /portal/v1/auth/device-approvals`: the visitor approves a launcher's user code.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    pub struct DeviceApproval {
        /// A fresh ID token of the visitor.
        pub id_token: String,
        /// `XXXX-XXXX` as typed (case- and dash-insensitive).
        pub user_code: String,
    }

    /// `POST /portal/v1/auth/device-approvals` → 200; the launcher's next poll signs in.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DeviceApproved {
        /// The device that asked, for the confirmation page.
        pub device_label: String,
        /// `windows`, `proton`, `linux`.
        pub platform: String,
    }
}

/// Retired routes for the frozen fake server only.
pub mod routes {
    /// Historical launcher route constants.
    pub mod launcher {
        pub use crate::routes::launcher::*;
        /// Retired device-code endpoint.
        pub const AUTH_DEVICE: &str = "/launcher/v1/auth/device";
    }
    /// Historical portal route constants.
    pub mod portal {
        pub use crate::routes::portal::*;
        /// Retired loopback authorization endpoint.
        pub const AUTH_LAUNCHER_CODES: &str = "/portal/v1/auth/launcher-codes";
        /// Retired device approval endpoint.
        pub const AUTH_DEVICE_APPROVALS: &str = "/portal/v1/auth/device-approvals";
    }
}
