//! The ID tokens of a `local` instance's built-in e-mail and password provider (ACCOUNTS.md §4.1
//! "Two identity backends", ADR 0023 §9, OD-122): PASETO `v4.public` signed by
//! `FI_LOCAL_IDP_SIGNING_KEYS` with a `kid` footer, exactly as [`crate::access`] signs access
//! tokens (current key signs, every key of the ring verifies). Opaque to every client: the
//! launcher only forwards one to `POST /launcher/v1/auth/token`.
//!
//! Claims: `iss = <FI_PUBLIC_CORE_URL>/launcher/v1/auth`, `aud = <realm>`, `sub` = the credential
//! id (a UUID), `email`, `email_verified` (whether the address is confirmed, CR-S53; always true on
//! an instance that delivers no mail), `sign_in_provider: "password"`, and `iat`,
//! `auth_time`, `exp = iat + 300` as Unix seconds (the shape of Identity Platform's ID tokens).
//!
//! [`LocalIdp::verify`] answers the same [`Identity`] and [`IdTokenError`] as
//! [`crate::idtoken::Verifier::verify`], with the same checks and order: everything up to the
//! claims' times is `Invalid`, then the provider allowlist (`FI_IDP_PROVIDERS`), then
//! `email_verified`. [`crate::idtoken::is_recent`] applies unchanged to its `auth_time`.

use crate::idtoken::{FRESH_SECS, IdTokenError, Identity, SKEW_SECS};
use crate::keys::SigningKeys;
use crate::{Secret, paseto};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Lifetime of a local ID token (5 minutes).
pub const TTL_SECS: i64 = 300;
/// The provider every local ID token names.
pub const PROVIDER: &str = "password";
/// The issuer's path under the public core origin.
pub const ISSUER_PATH: &str = "/launcher/v1/auth";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Claims {
    iss: String,
    aud: String,
    sub: String,
    email: String,
    email_verified: bool,
    sign_in_provider: String,
    iat: i64,
    auth_time: i64,
    exp: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Footer {
    kid: String,
}

/// A local provider that cannot start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the realm and at least one provider are required")]
pub struct ConfigError;

/// The local provider of one instance: signs and verifies its ID tokens.
#[derive(Clone)]
pub struct LocalIdp {
    keys: SigningKeys,
    issuer: String,
    audience: String,
    providers: Vec<String>,
}

impl fmt::Debug for LocalIdp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalIdp")
            .field("keys", &self.keys)
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

/// A minted local ID token.
#[derive(Debug)]
pub struct Issued {
    /// The token (a credential for 5 minutes).
    pub token: Secret,
    /// Its life in seconds ([`TTL_SECS`]).
    pub expires_in: u32,
}

impl LocalIdp {
    /// The provider of an instance whose public core origin is `core_origin` (empty locally) and
    /// whose realm is `realm`, accepting `providers` (`FI_IDP_PROVIDERS`).
    pub fn new(
        keys: SigningKeys,
        core_origin: &str,
        realm: &str,
        providers: &[String],
    ) -> Result<Self, ConfigError> {
        if realm.is_empty() || providers.is_empty() || providers.iter().any(String::is_empty) {
            return Err(ConfigError);
        }
        Ok(Self {
            keys,
            issuer: format!("{}{ISSUER_PATH}", core_origin.trim_end_matches('/')),
            audience: realm.to_owned(),
            providers: providers.to_vec(),
        })
    }

    /// The `iss` of this instance's tokens.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Mints an ID token for credential `sub` with a confirmed `email` at `now` (Unix seconds);
    /// `auth_time` is when the password was last checked (now, for sign-up and sign-in).
    pub fn issue(&self, sub: &str, email: &str, auth_time: i64, now: i64) -> Issued {
        self.issue_with(sub, email, true, auth_time, now)
    }

    /// [`LocalIdp::issue`] saying whether the address is confirmed: an unconfirmed one verifies
    /// only through [`LocalIdp::verify_unconfirmed`] (the confirmation mail's resend), and
    /// [`LocalIdp::verify`] answers it `EmailUnverified` (`AUTH.EMAIL_UNVERIFIED`, as Identity
    /// Platform's tokens of an unverified password user).
    pub fn issue_with(
        &self,
        sub: &str,
        email: &str,
        email_verified: bool,
        auth_time: i64,
        now: i64,
    ) -> Issued {
        let (kid, key) = self.keys.current();
        let claims = Claims {
            iss: self.issuer.clone(),
            aud: self.audience.clone(),
            sub: sub.to_owned(),
            email: email.to_owned(),
            email_verified,
            sign_in_provider: PROVIDER.to_owned(),
            iat: now,
            auth_time,
            exp: now + TTL_SECS,
        };
        let message = serde_json::to_vec(&claims).unwrap_or_default();
        let footer = serde_json::to_vec(&Footer {
            kid: kid.to_owned(),
        })
        .unwrap_or_default();
        Issued {
            token: Secret::new(paseto::sign(key, &message, &footer, b"")),
            expires_in: TTL_SECS as u32,
        }
    }

    /// Verifies `token` at `now` (Unix seconds).
    pub fn verify(&self, token: &str, now: i64) -> Result<Identity, IdTokenError> {
        let claims = self.verified_claims(token, now)?;
        if !self.providers.contains(&claims.sign_in_provider) {
            return Err(IdTokenError::ProviderNotAllowed);
        }
        if !claims.email_verified {
            return Err(IdTokenError::EmailUnverified);
        }
        Ok(Identity {
            uid: claims.sub,
            provider_id: claims.sign_in_provider,
            email: Some(claims.email),
            email_verified: true,
            auth_time: claims.auth_time,
            issued_at: claims.iat,
        })
    }

    /// [`LocalIdp::verify`] that also accepts an unconfirmed address (only the resend of the
    /// confirmation mail uses it); `Identity::email_verified` says which.
    pub fn verify_unconfirmed(&self, token: &str, now: i64) -> Result<Identity, IdTokenError> {
        let claims = self.verified_claims(token, now)?;
        if !self.providers.contains(&claims.sign_in_provider) {
            return Err(IdTokenError::ProviderNotAllowed);
        }
        Ok(Identity {
            uid: claims.sub,
            provider_id: claims.sign_in_provider,
            email: Some(claims.email),
            email_verified: claims.email_verified,
            auth_time: claims.auth_time,
            issued_at: claims.iat,
        })
    }

    fn verified_claims(&self, token: &str, now: i64) -> Result<Claims, IdTokenError> {
        let invalid = |_| IdTokenError::Invalid;
        let footer = paseto::untrusted_footer(token).ok_or(IdTokenError::Invalid)?;
        let kid = serde_json::from_slice::<Footer>(&footer)
            .map_err(invalid)?
            .kid;
        let key = self.keys.verifying_key(&kid).ok_or(IdTokenError::Invalid)?;
        let message =
            paseto::verify(&key, token, &footer, b"").map_err(|_| IdTokenError::Invalid)?;
        let claims: Claims = serde_json::from_slice(&message).map_err(invalid)?;
        let ok = claims.iss == self.issuer
            && claims.aud == self.audience
            && claims.exp > now
            && claims.exp > claims.iat
            && claims.exp - claims.iat <= TTL_SECS
            && claims.iat <= now + SKEW_SECS
            && claims.auth_time <= now + SKEW_SECS
            && claims.auth_time <= claims.iat
            && claims.iat >= now - FRESH_SECS
            && uuid_shaped(&claims.sub);
        if ok {
            Ok(claims)
        } else {
            Err(IdTokenError::Invalid)
        }
    }
}

/// `8-4-4-4-12` lower-case hex (a credential id as Postgres prints it).
fn uuid_shaped(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::b64;
    use crate::idtoken::is_recent;

    const NOW: i64 = 1_792_245_600;
    const SUB: &str = "0192f0c4-7a3e-7b8c-9d0e-1f2a3b4c5d6e";

    fn ring(spec: &[(&str, u8)]) -> SigningKeys {
        let text: Vec<String> = spec
            .iter()
            .map(|(kid, b)| format!("{kid}:{}", b64::std_encode(&[*b; 32])))
            .collect();
        SigningKeys::parse("FI_LOCAL_IDP_SIGNING_KEYS", &text.join(",")).unwrap()
    }

    fn idp(keys: SigningKeys) -> LocalIdp {
        LocalIdp::new(
            keys,
            "https://api.example.test/",
            "realm-a",
            &["password".to_owned()],
        )
        .unwrap()
    }

    #[test]
    fn issues_and_verifies_through_a_rotation() {
        let old = idp(ring(&[("idp-old", 1)]));
        assert_eq!(old.issuer(), "https://api.example.test/launcher/v1/auth");
        let t = old.issue(SUB, "a@example.test", NOW, NOW);
        assert_eq!(t.expires_in, 300);
        assert!(!format!("{t:?}").contains(t.token.expose()));
        let rotated = idp(ring(&[("idp-new", 2), ("idp-old", 1)]));
        let id = rotated.verify(t.token.expose(), NOW + 60).unwrap();
        assert_eq!(
            (
                id.uid.as_str(),
                id.provider_id.as_str(),
                id.email.as_deref()
            ),
            (SUB, "password", Some("a@example.test"))
        );
        assert!(id.email_verified && id.auth_time == NOW && id.issued_at == NOW);
        assert!(is_recent(id.auth_time, NOW + 60) && !is_recent(id.auth_time, NOW + 301));
        assert!(!format!("{id:?}").contains("example.test"));
    }

    #[test]
    fn refuses_wrong_keys_times_issuer_audience_and_tampering() {
        let keys = ring(&[("idp-1", 3)]);
        let good = idp(keys.clone());
        let token = good.issue(SUB, "a@example.test", NOW, NOW).token;
        let token = token.expose();
        let invalid = Err(IdTokenError::Invalid);
        // Another key under the same kid, another ring.
        assert_eq!(idp(ring(&[("idp-1", 4)])).verify(token, NOW), invalid);
        assert_eq!(idp(ring(&[("idp-2", 3)])).verify(token, NOW), invalid);
        // Expired, older than the exchange window, from the future.
        assert_eq!(good.verify(token, NOW + 300), invalid);
        assert_eq!(good.verify(token, NOW + 301), invalid);
        assert_eq!(good.verify(token, NOW - 61), invalid);
        assert!(good.verify(token, NOW + 299).is_ok());
        // Another instance's issuer or realm.
        let other_iss = LocalIdp::new(
            keys.clone(),
            "https://other.test",
            "realm-a",
            &["password".into()],
        )
        .unwrap();
        assert_eq!(other_iss.verify(token, NOW), invalid);
        let other_aud = LocalIdp::new(
            keys.clone(),
            "https://api.example.test",
            "realm-b",
            &["password".into()],
        )
        .unwrap();
        assert_eq!(other_aud.verify(token, NOW), invalid);
        // Tampered body, footer or signature; not PASETO at all.
        assert_eq!(good.verify(&format!("{token}x"), NOW), invalid);
        let (body, footer) = token.rsplit_once('.').unwrap();
        let forged_footer = b64::url_encode(br#"{"kid":"idp-2"}"#);
        assert_eq!(
            good.verify(&format!("{body}.{forged_footer}"), NOW),
            invalid
        );
        let _ = footer;
        let mut bytes = token.as_bytes().to_vec();
        bytes[20] = if bytes[20] == b'A' { b'B' } else { b'A' };
        assert_eq!(
            good.verify(std::str::from_utf8(&bytes).unwrap(), NOW),
            invalid
        );
        assert_eq!(good.verify("not-a-token", NOW), invalid);
        // An access token of the same ring is not an ID token.
        let access = crate::access::issue(
            &keys,
            SUB,
            SUB,
            crate::access::Audience::Launcher,
            "player",
            NOW,
        );
        assert_eq!(good.verify(access.token.expose(), NOW), invalid);
        // A sub that is not a credential id.
        let odd = good.issue("not-a-uuid", "a@example.test", NOW, NOW).token;
        assert_eq!(good.verify(odd.expose(), NOW), invalid);
    }

    #[test]
    fn an_unconfirmed_address_verifies_only_for_the_resend() {
        let good = idp(ring(&[("idp-1", 6)]));
        let t = good
            .issue_with(SUB, "a@example.test", false, NOW, NOW)
            .token;
        assert_eq!(
            good.verify(t.expose(), NOW),
            Err(IdTokenError::EmailUnverified)
        );
        let id = good.verify_unconfirmed(t.expose(), NOW).unwrap();
        assert!(!id.email_verified && id.uid == SUB);
        let ok = good.issue(SUB, "a@example.test", NOW, NOW).token;
        assert!(
            good.verify_unconfirmed(ok.expose(), NOW)
                .unwrap()
                .email_verified
        );
        assert_eq!(
            good.verify_unconfirmed("not-a-token", NOW),
            Err(IdTokenError::Invalid)
        );
    }

    #[test]
    fn the_provider_allowlist_applies() {
        let keys = ring(&[("idp-1", 5)]);
        let t = idp(keys.clone())
            .issue(SUB, "a@example.test", NOW, NOW)
            .token;
        let oidc_only = LocalIdp::new(
            keys.clone(),
            "https://api.example.test",
            "realm-a",
            &["oidc.x".into()],
        )
        .unwrap();
        assert_eq!(
            oidc_only.verify(t.expose(), NOW),
            Err(IdTokenError::ProviderNotAllowed)
        );
        assert!(LocalIdp::new(keys.clone(), "", "", &["password".into()]).is_err());
        assert!(LocalIdp::new(keys, "", "realm", &[]).is_err());
    }
}
