//! The site-key pepper (Secret Manager `site-key-pepper`, versioned; ACCOUNTS.md §4.2, §9, §10):
//! HMAC-SHA256 keys that make stored values useless without the secret.
//!
//! - **Legacy site keys.** Beta stored `SHA-1(canonical key)` unsalted (searchable in a 60-bit
//!   space). The importer stores `legacy_wrapped = HMAC(pepper_v, lower-hex(SHA-1(canonical)))`;
//!   the legacy link recomputes the same from what the player typed. New keys are never issued
//!   (OD-20), but a `key_hmac = HMAC(pepper_v, canonical)` row still verifies.
//! - **Rate-limit subjects** (`rate_limit.key`): `<limit>:<hex HMAC>` of the gamertag, uid, device
//!   or client network, so no subject is stored in clear.
//! - **Idempotency scopes** of routes that run before an account exists: `identity:<hex HMAC of
//!   the uid>`.
//!
//! The first version is current; older versions only verify (a rotation re-wraps rows lazily).

use crate::hex;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::fmt;

type HmacSha256 = Hmac<Sha256>;

/// The versioned pepper.
#[derive(Clone)]
pub struct Pepper {
    versions: Vec<(u16, [u8; 32])>,
}

impl fmt::Debug for Pepper {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let versions: Vec<u16> = self.versions.iter().map(|(v, _)| *v).collect();
        f.debug_struct("Pepper")
            .field("versions", &versions)
            .finish_non_exhaustive()
    }
}

/// A pepper configuration that cannot be used (the message never contains a secret).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("site-key pepper: {0}")]
pub struct PepperError(pub &'static str);

fn mac(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes keys of any length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// Lower-case hex of SHA-1(`canonical`): Beta's stored form of a site key.
pub fn beta_sha1_hex(canonical: &str) -> String {
    use sha1::Digest;
    hex(&sha1::Sha1::digest(canonical.as_bytes()))
}

impl Pepper {
    /// From `(version, secret)` pairs, current first; versions are positive and unique.
    pub fn new(versions: Vec<(u16, [u8; 32])>) -> Result<Self, PepperError> {
        if versions.is_empty() {
            return Err(PepperError("no version"));
        }
        if versions.iter().any(|(v, _)| *v == 0) {
            return Err(PepperError("versions start at 1"));
        }
        let mut seen: Vec<u16> = versions.iter().map(|(v, _)| *v).collect();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() != versions.len() {
            return Err(PepperError("a version appears twice"));
        }
        Ok(Self { versions })
    }

    /// The version new values are written with.
    pub fn current_version(&self) -> u16 {
        self.versions[0].0
    }

    fn secret(&self, version: u16) -> Option<&[u8; 32]> {
        self.versions
            .iter()
            .find(|(v, _)| *v == version)
            .map(|(_, s)| s)
    }

    fn current(&self) -> &[u8; 32] {
        &self.versions[0].1
    }

    /// Every secret, for the refusal of well-known local values in production.
    pub fn secrets(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.versions.iter().map(|(_, s)| s)
    }

    /// `HMAC(pepper_v, lower-hex(SHA-1(canonical)))`: the imported Beta key's stored form.
    pub fn legacy_wrapped(&self, version: u16, canonical: &str) -> Option<[u8; 32]> {
        let secret = self.secret(version)?;
        Some(mac(secret, &[beta_sha1_hex(canonical).as_bytes()]))
    }

    /// `HMAC(pepper_v, canonical)`.
    pub fn key_hmac(&self, version: u16, canonical: &str) -> Option<[u8; 32]> {
        let secret = self.secret(version)?;
        Some(mac(secret, &[canonical.as_bytes()]))
    }

    /// `rate_limit.key` for `subject` under `limit` (`^[a-z][a-z0-9_.]*$`, e.g. `signin.uid`).
    pub fn rate_limit_key(&self, limit: &str, subject: &str) -> String {
        debug_assert!(
            limit.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
                && limit.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'_'
                    || b == b'.')
        );
        let digest = mac(
            self.current(),
            &[
                b"fi-rate-limit-v1\n",
                limit.as_bytes(),
                b"\n",
                subject.as_bytes(),
            ],
        );
        format!("{limit}:{}", hex(&digest))
    }

    /// `device_binding.fingerprint_hmac` of a launcher's `Device.fingerprint` (CR-S44): only the
    /// HMAC is stored, so the value itself never rests in the database. Under the current pepper
    /// (a rotation starts new values; same-device history is best effort).
    pub fn device_fingerprint(&self, fingerprint: &str) -> [u8; 32] {
        mac(
            self.current(),
            &[b"fi-device-fingerprint-v1\n", fingerprint.as_bytes()],
        )
    }

    /// `idempotency_record.scope` of a `local` sign-up (OD-122): the normalized e-mail under its
    /// own label, so no address is stored in clear.
    pub fn signup_scope(&self, email_lower: &str) -> String {
        let digest = mac(
            self.current(),
            &[b"fi-idempotency-local-signup-v1\n", email_lower.as_bytes()],
        );
        format!("identity:{}", hex(&digest))
    }

    /// The `request_sha256` of a `local` sign-up's idempotency record: a keyed MAC over the route,
    /// the normalized e-mail and the password, so the record never holds a plain digest of a
    /// password that could be guessed offline.
    pub fn signup_request(&self, route: &str, email_lower: &str, password: &str) -> [u8; 32] {
        mac(
            self.current(),
            &[
                b"fi-idempotency-local-signup-request-v1\n",
                route.as_bytes(),
                b"\n",
                email_lower.as_bytes(),
                b"\n",
                password.as_bytes(),
            ],
        )
    }

    /// The stored MAC of a `local` instance's 6-digit confirmation code (CR-S53 follow-up "6-digit
    /// code"), keyed by the pepper (not in the database) and bound to its mail token's row
    /// (`token_sha256`), so a dump of the table alone cannot test codes. Under the current pepper:
    /// a rotation within the code's 24 hours makes it fail (the player sends a new mail).
    pub fn confirmation_code(&self, token_sha256: &[u8; 32], code: &str) -> [u8; 32] {
        mac(
            self.current(),
            &[
                b"fi-local-confirmation-code-v1\n",
                token_sha256.as_slice(),
                b"\n",
                code.as_bytes(),
            ],
        )
    }

    /// `idempotency_record.scope` of an identity that has no account yet.
    pub fn identity_scope(&self, idp_uid: &str) -> String {
        let digest = mac(
            self.current(),
            &[b"fi-idempotency-identity-v1\n", idp_uid.as_bytes()],
        );
        format!("identity:{}", hex(&digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pepper() -> Pepper {
        Pepper::new(vec![(2, [7u8; 32]), (1, [9u8; 32])]).unwrap()
    }

    #[test]
    fn versions_are_checked() {
        assert!(Pepper::new(vec![]).is_err());
        assert!(Pepper::new(vec![(0, [1; 32])]).is_err());
        assert!(Pepper::new(vec![(1, [1; 32]), (1, [2; 32])]).is_err());
        assert_eq!(pepper().current_version(), 2);
        assert!(
            !format!("{:?}", pepper()).contains('7'),
            "no secret in Debug"
        );
    }

    #[test]
    fn legacy_keys_wrap_beta_sha1_hex() {
        let p = pepper();
        // SHA-1("abc") is a9993e36...: the wrap is HMAC over that hex text, per version.
        assert_eq!(
            beta_sha1_hex("abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let v2 = p.legacy_wrapped(2, "K7QM4XPA9RT2").unwrap();
        assert_eq!(
            v2,
            mac(&[7u8; 32], &[beta_sha1_hex("K7QM4XPA9RT2").as_bytes()])
        );
        assert_ne!(Some(v2), p.legacy_wrapped(1, "K7QM4XPA9RT2"));
        assert_ne!(Some(v2), p.legacy_wrapped(2, "K7QM4XPA9RT3"));
        assert_eq!(p.legacy_wrapped(3, "K7QM4XPA9RT2"), None);
        assert_ne!(p.key_hmac(2, "K7QM4XPA9RT2"), Some(v2));
    }

    #[test]
    fn subjects_are_keyed_and_never_in_clear() {
        let p = pepper();
        let key = p.rate_limit_key("signin.net", "v4:203.0.113");
        assert!(key.starts_with("signin.net:") && key.len() == "signin.net:".len() + 64);
        assert!(!key.contains("203"));
        assert_ne!(key, p.rate_limit_key("create.net", "v4:203.0.113"));
        assert_ne!(key, p.rate_limit_key("signin.net", "v4:203.0.114"));
        let scope = p.identity_scope("uid-1");
        assert!(scope.starts_with("identity:") && scope.len() == 9 + 64);
        assert_ne!(scope, p.identity_scope("uid-2"));
    }
}
