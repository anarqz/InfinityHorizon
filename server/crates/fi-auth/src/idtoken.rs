//! Identity Platform (Firebase Authentication) ID-token verification, ACCOUNTS.md §4.1.
//!
//! Every exchange checks, in this order, and answers one code without saying which check failed:
//! 1. the compact JWT shape and header: `alg = RS256` with a `kid` the [`KeySet`] holds and a
//!    signature that verifies over `header.payload` ([`Mode::Rs256`]), or, in [`Mode::Emulator`]
//!    only, `alg = none` with an empty signature (the Firebase Auth emulator's tokens);
//! 2. `iss = https://securetoken.google.com/<project>`, `aud = <project>`, `exp > now`,
//!    `iat ≤ now + 60 s`, `auth_time ≤ now + 60 s`, `iat ≥ now − 5 min` (clients force-refresh
//!    right before an exchange), `sub` 1-128 characters, no `firebase.tenant`;
//!    any failure so far is [`IdTokenError::Invalid`] (`AUTH.ID_TOKEN_INVALID`);
//! 3. `firebase.sign_in_provider` in the allowlist, else [`IdTokenError::ProviderNotAllowed`];
//! 4. `password` sign-ins need `email_verified = true`, else [`IdTokenError::EmailUnverified`].
//!
//! [`is_recent`] is the separate "recent sign-in" rule (`auth_time ≥ now − 5 min`) of deletion and
//! legacy link (`AUTH.REAUTH_REQUIRED`).
//!
//! RS256 uses `jsonwebtoken`'s pure-Rust backend for the signature only; the claim checks are
//! here, identical in both modes. Google's key set is fetched by fi-api (cached per
//! `Cache-Control`); [`StaticKeys`] holds a fixed set (tests, and the fetcher's cache).

use crate::b64;
use jsonwebtoken::{Algorithm, DecodingKey};
use serde::Deserialize;
use std::fmt;
use std::sync::Arc;

/// Accepted clock difference for `iat` and `auth_time` in the future.
pub const SKEW_SECS: i64 = 60;
/// Oldest `iat` accepted for an exchange, and the "recent sign-in" window.
pub const FRESH_SECS: i64 = 300;
/// Longest `sub` (Identity Platform `localId`).
pub const MAX_UID_CHARS: usize = 128;

/// Public keys by `kid` (Google's `securetoken@system.gserviceaccount.com` JWKS).
pub trait KeySet: Send + Sync {
    /// The RS256 key named `kid`, if the set holds it.
    fn key(&self, kid: &str) -> Option<DecodingKey>;
}

/// A fixed key set.
#[derive(Clone, Default)]
pub struct StaticKeys {
    keys: Vec<(String, DecodingKey)>,
}

impl fmt::Debug for StaticKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kids: Vec<&str> = self.keys.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("StaticKeys").field("kids", &kids).finish()
    }
}

/// A JWKS document that does not parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a JWKS of RSA keys")]
pub struct JwksError;

impl StaticKeys {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an RSA key from its base64url modulus and exponent (a JWK's `n` and `e`).
    pub fn with_rsa(mut self, kid: &str, n: &str, e: &str) -> Result<Self, JwksError> {
        let (n, e) = (
            b64::url_decode(n).ok_or(JwksError)?,
            b64::url_decode(e).ok_or(JwksError)?,
        );
        if n.len() < 256 || e.is_empty() {
            return Err(JwksError);
        }
        self.keys
            .push((kid.to_owned(), DecodingKey::from_rsa_raw_components(&n, &e)));
        Ok(self)
    }

    /// Parses a JWKS document (`{"keys": [{"kid", "kty": "RSA", "n", "e", ...}]}`); keys other
    /// than RSA are skipped.
    pub fn from_jwks(json: &str) -> Result<Self, JwksError> {
        #[derive(Deserialize)]
        struct Jwk {
            kid: String,
            kty: String,
            n: Option<String>,
            e: Option<String>,
        }
        #[derive(Deserialize)]
        struct Jwks {
            keys: Vec<Jwk>,
        }
        let jwks: Jwks = serde_json::from_str(json).map_err(|_| JwksError)?;
        let mut set = Self::new();
        for k in jwks.keys.into_iter().filter(|k| k.kty == "RSA") {
            set = set.with_rsa(
                &k.kid,
                k.n.as_deref().ok_or(JwksError)?,
                k.e.as_deref().ok_or(JwksError)?,
            )?;
        }
        Ok(set)
    }
}

impl KeySet for StaticKeys {
    fn key(&self, kid: &str) -> Option<DecodingKey> {
        self.keys
            .iter()
            .find(|(k, _)| k == kid)
            .map(|(_, key)| key.clone())
    }
}

/// How signatures are checked.
#[derive(Clone)]
pub enum Mode {
    /// Production: RS256 against Google's keys.
    Rs256(Arc<dyn KeySet>),
    /// Local only (`FIREBASE_AUTH_EMULATOR_HOST`): the emulator's unsigned tokens; refused for a
    /// project id without the `demo-` prefix (fi-api also refuses it with `APP_ENV=prod`).
    Emulator,
}

impl fmt::Debug for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Rs256(_) => "Rs256",
            Mode::Emulator => "Emulator",
        })
    }
}

/// A verifier configuration that must not start.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Emulator mode with a project that could be real.
    #[error("emulator mode needs a demo- project id")]
    EmulatorNeedsDemoProject,
    /// Empty project or provider list.
    #[error("the project id and at least one provider are required")]
    Empty,
}

/// Why an ID token was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum IdTokenError {
    /// `AUTH.ID_TOKEN_INVALID`.
    #[error("invalid ID token")]
    Invalid,
    /// `AUTH.PROVIDER_NOT_ALLOWED`.
    #[error("sign-in provider not allowed")]
    ProviderNotAllowed,
    /// `AUTH.EMAIL_UNVERIFIED`.
    #[error("e-mail not verified")]
    EmailUnverified,
}

/// A verified identity.
#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    /// Identity Platform uid (`sub`).
    pub uid: String,
    /// `firebase.sign_in_provider`, one of the allowlist (`password` at launch, OD-42).
    pub provider_id: String,
    /// The e-mail, if the token has one.
    pub email: Option<String>,
    /// `email_verified`.
    pub email_verified: bool,
    /// `auth_time` (Unix seconds): when the person last actually signed in.
    pub auth_time: i64,
    /// `iat` (Unix seconds).
    pub issued_at: i64,
}

impl fmt::Debug for Identity {
    // Never prints the e-mail or the uid (personal data; logs carry ids we mint, not these).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity")
            .field("provider_id", &self.provider_id)
            .field("email_verified", &self.email_verified)
            .field("auth_time", &self.auth_time)
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// The e-mail in lower case, for storage (`account_identity.email_lower`).
    pub fn email_lower(&self) -> Option<String> {
        self.email
            .as_deref()
            .map(str::to_lowercase)
            .filter(|e| (3..=320).contains(&e.chars().count()))
    }
}

/// Whether a sign-in at `auth_time` is recent enough for deletion and legacy link.
pub fn is_recent(auth_time: i64, now: i64) -> bool {
    auth_time >= now - FRESH_SECS && auth_time <= now + SKEW_SECS
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize)]
struct Firebase {
    sign_in_provider: String,
    tenant: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    aud: String,
    exp: i64,
    iat: i64,
    auth_time: i64,
    sub: String,
    email: Option<String>,
    email_verified: Option<bool>,
    firebase: Firebase,
}

/// The ID-token verifier of one Identity Platform project.
#[derive(Clone, Debug)]
pub struct Verifier {
    mode: Mode,
    project_id: String,
    issuer: String,
    providers: Vec<String>,
}

impl Verifier {
    /// A verifier for `project_id` accepting `providers` (`FI_IDP_PROVIDERS`).
    pub fn new(mode: Mode, project_id: &str, providers: &[String]) -> Result<Self, ConfigError> {
        if project_id.is_empty() || providers.is_empty() || providers.iter().any(String::is_empty) {
            return Err(ConfigError::Empty);
        }
        if matches!(mode, Mode::Emulator) && !project_id.starts_with("demo-") {
            return Err(ConfigError::EmulatorNeedsDemoProject);
        }
        Ok(Self {
            mode,
            project_id: project_id.to_owned(),
            issuer: format!("https://securetoken.google.com/{project_id}"),
            providers: providers.to_vec(),
        })
    }

    /// Whether this verifier accepts the emulator's unsigned tokens.
    pub fn is_emulator(&self) -> bool {
        matches!(self.mode, Mode::Emulator)
    }

    /// Verifies `token` at `now` (Unix seconds).
    pub fn verify(&self, token: &str, now: i64) -> Result<Identity, IdTokenError> {
        let claims = self.verified_claims(token, now)?;
        if !self.providers.contains(&claims.firebase.sign_in_provider) {
            return Err(IdTokenError::ProviderNotAllowed);
        }
        let email_verified = claims.email_verified.unwrap_or(false);
        if claims.firebase.sign_in_provider == "password" && !email_verified {
            return Err(IdTokenError::EmailUnverified);
        }
        Ok(Identity {
            uid: claims.sub,
            provider_id: claims.firebase.sign_in_provider,
            email: claims.email,
            email_verified,
            auth_time: claims.auth_time,
            issued_at: claims.iat,
        })
    }

    /// Steps 1 and 2: shape, signature, issuer, audience, times, subject, tenant.
    fn verified_claims(&self, token: &str, now: i64) -> Result<Claims, IdTokenError> {
        let invalid = |_| IdTokenError::Invalid;
        let mut parts = token.split('.');
        let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(IdTokenError::Invalid);
        };
        let header: Header = b64::url_decode(header_b64)
            .ok_or(IdTokenError::Invalid)
            .and_then(|h| serde_json::from_slice(&h).map_err(invalid))?;
        match &self.mode {
            Mode::Emulator => {
                if header.alg != "none" || !signature_b64.is_empty() {
                    return Err(IdTokenError::Invalid);
                }
            }
            Mode::Rs256(keys) => {
                if header.alg != "RS256" || signature_b64.is_empty() {
                    return Err(IdTokenError::Invalid);
                }
                let key = header
                    .kid
                    .as_deref()
                    .and_then(|kid| keys.key(kid))
                    .ok_or(IdTokenError::Invalid)?;
                let signed = &token[..header_b64.len() + 1 + payload_b64.len()];
                let ok = jsonwebtoken::crypto::verify(
                    signature_b64,
                    signed.as_bytes(),
                    &key,
                    Algorithm::RS256,
                )
                .unwrap_or(false);
                if !ok {
                    return Err(IdTokenError::Invalid);
                }
            }
        }
        let claims: Claims = b64::url_decode(payload_b64)
            .ok_or(IdTokenError::Invalid)
            .and_then(|p| serde_json::from_slice(&p).map_err(invalid))?;
        let sub_chars = claims.sub.chars().count();
        let ok = claims.iss == self.issuer
            && claims.aud == self.project_id
            && claims.exp > now
            && claims.iat <= now + SKEW_SECS
            && claims.auth_time <= now + SKEW_SECS
            && claims.iat >= now - FRESH_SECS
            && (1..=MAX_UID_CHARS).contains(&sub_chars)
            && claims.firebase.tenant.is_none();
        if ok {
            Ok(claims)
        } else {
            Err(IdTokenError::Invalid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::EncodingKey;
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use serde_json::{Value, json};
    use std::sync::OnceLock;

    const NOW: i64 = 1_792_245_600;
    const PROJECT: &str = "demo-forza-infinity";

    /// One 2048-bit key per test process, generated in code (no key files).
    fn rsa_key() -> &'static (EncodingKey, String, String) {
        static KEY: OnceLock<(EncodingKey, String, String)> = OnceLock::new();
        KEY.get_or_init(|| {
            let mut rng = rsa::rand_core::OsRng;
            let private = rsa::RsaPrivateKey::new(&mut rng, 2048).expect("RSA key generation");
            let der = private.to_pkcs1_der().expect("PKCS#1 DER");
            let public = private.to_public_key();
            (
                EncodingKey::from_rsa_der(der.as_bytes()),
                b64::url_encode(&public.n().to_bytes_be()),
                b64::url_encode(&public.e().to_bytes_be()),
            )
        })
    }

    fn claims() -> Value {
        json!({
            "iss": format!("https://securetoken.google.com/{PROJECT}"),
            "aud": PROJECT,
            "auth_time": NOW - 10,
            "user_id": "uid-1",
            "sub": "uid-1",
            "iat": NOW - 10,
            "exp": NOW + 3_590,
            "email": "Player@Example.test",
            "email_verified": true,
            "firebase": {"identities": {"email": ["player@example.test"]}, "sign_in_provider": "password"}
        })
    }

    fn unsigned(claims: &Value) -> String {
        format!(
            "{}.{}.",
            b64::url_encode(br#"{"alg":"none","typ":"JWT"}"#),
            b64::url_encode(&serde_json::to_vec(claims).unwrap())
        )
    }

    fn signed(claims: &Value, kid: &str) -> String {
        let header = format!(r#"{{"alg":"RS256","kid":"{kid}","typ":"JWT"}}"#);
        let input = format!(
            "{}.{}",
            b64::url_encode(header.as_bytes()),
            b64::url_encode(&serde_json::to_vec(claims).unwrap())
        );
        let signature =
            jsonwebtoken::crypto::sign(input.as_bytes(), &rsa_key().0, Algorithm::RS256).unwrap();
        format!("{input}.{signature}")
    }

    /// The configured allowlist (OD-42: e-mail and password only).
    fn providers() -> Vec<String> {
        vec!["password".into()]
    }

    fn emulator() -> Verifier {
        Verifier::new(Mode::Emulator, PROJECT, &providers()).unwrap()
    }

    fn production() -> Verifier {
        let (_, n, e) = rsa_key();
        let keys = StaticKeys::new().with_rsa("k1", n, e).unwrap();
        Verifier::new(Mode::Rs256(Arc::new(keys)), PROJECT, &providers()).unwrap()
    }

    fn with(mut c: Value, path: &[&str], value: Value) -> Value {
        let mut slot = &mut c;
        for key in &path[..path.len() - 1] {
            slot = &mut slot[*key];
        }
        let last = path[path.len() - 1];
        if value.is_null() {
            slot.as_object_mut().unwrap().remove(last);
        } else {
            slot[last] = value;
        }
        c
    }

    #[test]
    fn emulator_tokens_verify_in_emulator_mode_only() {
        let token = unsigned(&claims());
        let id = emulator().verify(&token, NOW).unwrap();
        assert_eq!(id.uid, "uid-1");
        assert_eq!(id.provider_id, "password");
        assert_eq!(id.email_lower().as_deref(), Some("player@example.test"));
        assert!(!format!("{id:?}").contains("uid-1") && !format!("{id:?}").contains("xample"));
        assert_eq!(production().verify(&token, NOW), Err(IdTokenError::Invalid));
        assert_eq!(
            Verifier::new(Mode::Emulator, "example-project", &providers()).unwrap_err(),
            ConfigError::EmulatorNeedsDemoProject
        );
        // An emulator verifier refuses a token that carries a signature.
        let with_sig = format!("{token}c2ln");
        assert_eq!(
            emulator().verify(&with_sig, NOW),
            Err(IdTokenError::Invalid)
        );
    }

    #[test]
    fn rs256_tokens_verify_with_the_key_set() {
        let v = production();
        let token = signed(&claims(), "k1");
        assert_eq!(v.verify(&token, NOW).unwrap().uid, "uid-1");
        assert_eq!(
            v.verify(&signed(&claims(), "unknown"), NOW),
            Err(IdTokenError::Invalid)
        );
        assert_eq!(
            emulator().verify(&token, NOW),
            Err(IdTokenError::Invalid),
            "the emulator verifier never accepts RS256 tokens it cannot check"
        );
        // Tampered payload, same signature.
        let (head, rest) = token.split_once('.').unwrap();
        let (_, sig) = rest.split_once('.').unwrap();
        let forged = format!(
            "{head}.{}.{sig}",
            b64::url_encode(
                &serde_json::to_vec(&with(claims(), &["sub"], json!("uid-2"))).unwrap()
            )
        );
        assert_eq!(v.verify(&forged, NOW), Err(IdTokenError::Invalid));
        // alg confusion: an unsigned token presented to the production verifier.
        assert_eq!(
            v.verify(&unsigned(&claims()), NOW),
            Err(IdTokenError::Invalid)
        );
        let jwks = format!(
            r#"{{"keys":[{{"kid":"k1","kty":"RSA","alg":"RS256","use":"sig","n":"{}","e":"{}"}}]}}"#,
            rsa_key().1,
            rsa_key().2
        );
        let from_jwks = Verifier::new(
            Mode::Rs256(Arc::new(StaticKeys::from_jwks(&jwks).unwrap())),
            PROJECT,
            &providers(),
        )
        .unwrap();
        assert!(from_jwks.verify(&token, NOW).is_ok());
    }

    #[test]
    fn every_claim_check_fails_as_invalid() {
        let v = production();
        let cases: Vec<(&str, Value)> = vec![
            (
                "issuer",
                with(
                    claims(),
                    &["iss"],
                    json!("https://securetoken.google.com/other"),
                ),
            ),
            ("audience", with(claims(), &["aud"], json!("other"))),
            ("audience array", with(claims(), &["aud"], json!([PROJECT]))),
            ("expired", with(claims(), &["exp"], json!(NOW))),
            (
                "issued in the future",
                with(claims(), &["iat"], json!(NOW + 61)),
            ),
            (
                "signed in the future",
                with(claims(), &["auth_time"], json!(NOW + 61)),
            ),
            ("stale iat", with(claims(), &["iat"], json!(NOW - 301))),
            ("empty sub", with(claims(), &["sub"], json!(""))),
            ("long sub", with(claims(), &["sub"], json!("u".repeat(129)))),
            (
                "tenant",
                with(claims(), &["firebase", "tenant"], json!("t1")),
            ),
            (
                "no provider",
                with(claims(), &["firebase", "sign_in_provider"], Value::Null),
            ),
            ("no exp", with(claims(), &["exp"], Value::Null)),
        ];
        for (name, c) in cases {
            assert_eq!(
                v.verify(&signed(&c, "k1"), NOW),
                Err(IdTokenError::Invalid),
                "{name}"
            );
            assert_eq!(
                emulator().verify(&unsigned(&c), NOW),
                Err(IdTokenError::Invalid),
                "{name}"
            );
        }
        for garbage in ["", "a.b", "a.b.c.d", "...", "eyJ.eyJ.", "not a token"] {
            assert_eq!(
                v.verify(garbage, NOW),
                Err(IdTokenError::Invalid),
                "{garbage}"
            );
        }
        // Inside the skew and freshness windows.
        let edge = with(
            with(claims(), &["iat"], json!(NOW - 300)),
            &["auth_time"],
            json!(NOW + 60),
        );
        assert!(v.verify(&signed(&edge, "k1"), NOW).is_ok());
    }

    #[test]
    fn provider_and_email_rules() {
        let v = production();
        let apple = with(
            claims(),
            &["firebase", "sign_in_provider"],
            json!("apple.com"),
        );
        assert_eq!(
            v.verify(&signed(&apple, "k1"), NOW),
            Err(IdTokenError::ProviderNotAllowed)
        );
        let unverified = with(claims(), &["email_verified"], json!(false));
        assert_eq!(
            v.verify(&signed(&unverified, "k1"), NOW),
            Err(IdTokenError::EmailUnverified)
        );
        let missing = with(claims(), &["email_verified"], Value::Null);
        assert_eq!(
            v.verify(&signed(&missing, "k1"), NOW),
            Err(IdTokenError::EmailUnverified)
        );
        // OD-42: e-mail and password is the only provider, so a Google ID token is refused, with a
        // verified e-mail or without.
        let google = with(
            claims(),
            &["firebase", "sign_in_provider"],
            json!("google.com"),
        );
        for google in [
            google.clone(),
            with(google, &["email_verified"], json!(false)),
        ] {
            assert_eq!(
                v.verify(&signed(&google, "k1"), NOW),
                Err(IdTokenError::ProviderNotAllowed)
            );
            assert_eq!(
                emulator().verify(&unsigned(&google), NOW),
                Err(IdTokenError::ProviderNotAllowed)
            );
        }
        // A bad token with a disallowed provider is still just invalid (no oracle).
        let bad_and_apple = with(apple, &["aud"], json!("other"));
        assert_eq!(
            v.verify(&signed(&bad_and_apple, "k1"), NOW),
            Err(IdTokenError::Invalid)
        );
    }

    #[test]
    fn the_allowlist_is_configuration_and_only_passwords_need_a_verified_e_mail() {
        // A provider added to FI_IDP_PROVIDERS later is accepted without a client release; the
        // verified-e-mail rule stays specific to `password`.
        let (_, n, e) = rsa_key();
        let keys = StaticKeys::new().with_rsa("k1", n, e).unwrap();
        let wider = Verifier::new(
            Mode::Rs256(Arc::new(keys)),
            PROJECT,
            &["password".into(), "oidc.example".into()],
        )
        .unwrap();
        let oidc = with(
            with(
                claims(),
                &["firebase", "sign_in_provider"],
                json!("oidc.example"),
            ),
            &["email_verified"],
            json!(false),
        );
        assert_eq!(
            wider
                .verify(&signed(&oidc, "k1"), NOW)
                .map(|id| id.provider_id),
            Ok("oidc.example".to_owned()),
            "only password sign-ins need a verified e-mail"
        );
        assert!(production().verify(&signed(&oidc, "k1"), NOW).is_err());
        assert_eq!(
            wider.verify(&signed(&claims(), "k1"), NOW).map(|id| id.uid),
            Ok("uid-1".to_owned())
        );
    }

    #[test]
    fn recent_sign_in_window() {
        assert!(is_recent(NOW - 300, NOW));
        assert!(!is_recent(NOW - 301, NOW));
        assert!(is_recent(NOW + 60, NOW));
        assert!(!is_recent(NOW + 61, NOW));
    }
}
