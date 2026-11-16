//! Signing keys and the pepper as configuration (Secret Manager in production, environment
//! variables locally; ACCOUNTS.md §4.2):
//!
//! - `FI_TOKEN_SIGNING_KEYS`, `FI_LAUNCH_TICKET_SIGNING_KEYS`: `kid:base64(32-byte Ed25519 seed)`,
//!   comma separated, **current key first**; every key verifies, only the current one signs, so a
//!   rotation keeps the previous key for 30 days (two public keys accepted);
//! - `FI_LOCAL_IDP_SIGNING_KEYS` (a `local` instance's ID tokens, OD-122): the same ring format;
//! - `FI_SITE_KEY_PEPPER`: `version:base64(32 bytes)`, comma separated, current first.
//!
//! Errors name the variable and the problem, never a value. The well-known LOCAL values of
//! `.env.example` and docker-compose.yml (base64 of SHA-256 of a public label) are recognised by
//! [`is_local_dev_secret`], so a production start can refuse them.

use crate::pepper::Pepper;
use crate::{b64, sha256};
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::fmt;

/// A key configuration that cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{variable}: {reason}")]
pub struct KeyError {
    /// The variable.
    pub variable: &'static str,
    /// What is wrong (never the value).
    pub reason: &'static str,
}

/// The labels whose SHA-256 are the local development values (docker-compose.yml, .env.example).
pub const LOCAL_DEV_LABELS: [&str; 5] = [
    "forza-infinity local dev access-token signing key 1",
    "forza-infinity local dev launch-ticket signing key 1",
    "forza-infinity local dev site-key pepper 1",
    "forza-infinity local dev portal assertion secret",
    // `FI_LOCAL_IDP_SIGNING_KEYS` of local stacks and tests (OD-122 M2.3).
    "forza-infinity local dev local-idp signing key 1",
];

/// Whether `secret` is one of the well-known local development values.
pub fn is_local_dev_secret(secret: &[u8]) -> bool {
    LOCAL_DEV_LABELS
        .iter()
        .any(|label| sha256(label.as_bytes()).as_slice() == secret)
}

pub(crate) fn valid_kid(kid: &str) -> bool {
    (1..=32).contains(&kid.len())
        && kid
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && kid
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// An Ed25519 key ring: the current key signs, every key verifies.
#[derive(Clone)]
pub struct SigningKeys {
    keys: Vec<(String, SigningKey)>,
}

impl fmt::Debug for SigningKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kids: Vec<&str> = self.keys.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("SigningKeys")
            .field("kids", &kids)
            .finish_non_exhaustive()
    }
}

impl SigningKeys {
    /// From `(kid, seed)` pairs, current first.
    pub fn from_seeds(
        variable: &'static str,
        seeds: Vec<(String, [u8; 32])>,
    ) -> Result<Self, KeyError> {
        let err = |reason| KeyError { variable, reason };
        if seeds.is_empty() {
            return Err(err("no key"));
        }
        if seeds.iter().any(|(kid, _)| !valid_kid(kid)) {
            return Err(err("a kid is 1-32 lower-case letters, digits or dashes"));
        }
        let mut kids: Vec<&str> = seeds.iter().map(|(k, _)| k.as_str()).collect();
        kids.sort_unstable();
        kids.dedup();
        if kids.len() != seeds.len() {
            return Err(err("a kid appears twice"));
        }
        Ok(Self {
            keys: seeds
                .into_iter()
                .map(|(kid, seed)| (kid, SigningKey::from_bytes(&seed)))
                .collect(),
        })
    }

    /// Parses `kid:base64seed,kid:base64seed`.
    pub fn parse(variable: &'static str, text: &str) -> Result<Self, KeyError> {
        let mut seeds = Vec::new();
        for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
            let (kid, seed) = item.split_once(':').ok_or(KeyError {
                variable,
                reason: "each key is kid:base64(32-byte seed)",
            })?;
            let seed: [u8; 32] = b64::std_decode(seed.trim())
                .and_then(|s| s.try_into().ok())
                .ok_or(KeyError {
                    variable,
                    reason: "a seed is the standard base64 of 32 bytes",
                })?;
            seeds.push((kid.trim().to_owned(), seed));
        }
        Self::from_seeds(variable, seeds)
    }

    /// The signing key and its kid.
    pub fn current(&self) -> (&str, &SigningKey) {
        let (kid, key) = &self.keys[0];
        (kid, key)
    }

    /// The public key named `kid`.
    pub fn verifying_key(&self, kid: &str) -> Option<VerifyingKey> {
        self.keys
            .iter()
            .find(|(k, _)| k == kid)
            .map(|(_, s)| s.verifying_key())
    }

    /// Every `(kid, public key)`, current first (what a verifier embeds).
    pub fn public_keys(&self) -> Vec<(String, [u8; 32])> {
        self.keys
            .iter()
            .map(|(k, s)| (k.clone(), s.verifying_key().to_bytes()))
            .collect()
    }

    /// Whether any key is a well-known local development seed.
    pub fn has_local_dev_key(&self) -> bool {
        self.keys
            .iter()
            .any(|(_, s)| is_local_dev_secret(&s.to_bytes()))
    }
}

/// Parses `FI_SITE_KEY_PEPPER`: `version:base64(32 bytes)`, current first.
pub fn parse_pepper(variable: &'static str, text: &str) -> Result<Pepper, KeyError> {
    let mut versions = Vec::new();
    for item in text.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        let (version, secret) = item.split_once(':').ok_or(KeyError {
            variable,
            reason: "each pepper is version:base64(32 bytes)",
        })?;
        let version: u16 = version.trim().parse().map_err(|_| KeyError {
            variable,
            reason: "a version is a positive integer",
        })?;
        let secret: [u8; 32] = b64::std_decode(secret.trim())
            .and_then(|s| s.try_into().ok())
            .ok_or(KeyError {
                variable,
                reason: "a pepper is the standard base64 of 32 bytes",
            })?;
        versions.push((version, secret));
    }
    Pepper::new(versions).map_err(|e| KeyError {
        variable,
        reason: e.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL_TOKEN_KEYS: &str = "at-local-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=";

    #[test]
    fn parses_rings_current_first() {
        let second = b64::std_encode(&[5u8; 32]);
        let ring = SigningKeys::parse(
            "FI_TOKEN_SIGNING_KEYS",
            &format!("{LOCAL_TOKEN_KEYS}, at-2026-09:{second}"),
        )
        .unwrap();
        assert_eq!(ring.current().0, "at-local-1");
        assert!(ring.verifying_key("at-2026-09").is_some());
        assert!(ring.verifying_key("nope").is_none());
        assert_eq!(ring.public_keys().len(), 2);
        assert!(
            ring.has_local_dev_key(),
            "the compose default is recognised"
        );
        let prod = SigningKeys::parse("X", &format!("at-2026-10:{second}")).unwrap();
        assert!(!prod.has_local_dev_key());
        assert!(!format!("{ring:?}").contains("HI9J"), "no seed in Debug");
        let idp = SigningKeys::parse(
            "FI_LOCAL_IDP_SIGNING_KEYS",
            "idp-local-1:BvJcQZ87cw9bQswFPCv0HZSm7+67u1v9rhIWKh01i/o=",
        )
        .unwrap();
        assert!(idp.has_local_dev_key(), "the local IdP's dev key too");
    }

    #[test]
    fn bad_rings_name_the_variable_never_the_value() {
        for bad in [
            "",
            "nokid",
            "AT:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=",
            "at-1:c2hvcnQ=",
            "at-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=,at-1:HI9JRg7XdlATRFsJWlJBF/FM4Lk3kq2s8Lz9WcQ9BzM=",
        ] {
            let err = SigningKeys::parse("FI_TOKEN_SIGNING_KEYS", bad).unwrap_err();
            assert_eq!(err.variable, "FI_TOKEN_SIGNING_KEYS");
            assert!(!err.to_string().contains("HI9J"), "{err}");
        }
    }

    #[test]
    fn parses_the_pepper() {
        let p = parse_pepper(
            "FI_SITE_KEY_PEPPER",
            "1:84yC96SRqM8E+D3MCtARPCjLJ7eiFc/pGvdtLu2TaHQ=",
        )
        .unwrap();
        assert_eq!(p.current_version(), 1);
        assert!(p.secrets().all(|s| is_local_dev_secret(s)));
        assert!(parse_pepper("FI_SITE_KEY_PEPPER", "x:abc").is_err());
        assert!(
            parse_pepper(
                "FI_SITE_KEY_PEPPER",
                "0:84yC96SRqM8E+D3MCtARPCjLJ7eiFc/pGvdtLu2TaHQ="
            )
            .is_err()
        );
        assert!(!is_local_dev_secret(&[0u8; 32]));
    }
}
