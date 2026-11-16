//! Identity and token primitives of the account plane (ACCOUNTS.md §4, ADR 0006, ADR 0901).
//!
//! Pure: no I/O and no clock (every check takes `now`), so fi-api, its tests and tools run the same
//! code. Secrets never appear in `Debug` output or in error messages, and every refusal that a
//! client could probe answers without saying which check failed.
//!
//! | Module | What |
//! | --- | --- |
//! | [`idtoken`] | Identity Platform ID tokens: RS256 against a [`idtoken::KeySet`], or the Firebase Auth emulator's unsigned tokens |
//! | [`paseto`] | PASETO `v4.public` (Ed25519, PAE, footer, implicit assertion), checked with the specification's test vectors |
//! | [`access`] | Access tokens: claims, 15 minutes, `kid` footer, every key of the ring accepted |
//! | [`localidp`] | A `local` instance's own ID tokens: PASETO `v4.public`, 5 minutes, `kid` footer (OD-122) |
//! | [`password`] | A `local` instance's passwords: Argon2id, OWASP parameters, PHC strings (OD-122) |
//! | [`firebase_scrypt`] | Firebase Authentication's modified scrypt: imported users' hashes, rehashed to Argon2id at the first sign-in (OD-122) |
//! | [`ticket`] | Launch tickets: Ed25519 over `fi-launch-ticket-v1\n` ‖ canonical claims |
//! | [`opaque`] | Refresh tokens, identity tickets, portal session tokens: prefix + 256 random bits, SHA-256 lookup |
//! | [`codes`] | Portal hand-off codes (launcher → website) |
//! | [`pkce`] | RFC 7636 `S256` |
//! | [`pepper`] | Versioned HMAC-SHA256: legacy site keys, rate-limit subjects, idempotency scopes |
//! | [`mcp`] | MCP access tokens: `aud = mcp`, one hour, connection id and scopes (ADR 0018) |
//! | [`keys`] | `FI_TOKEN_SIGNING_KEYS`, `FI_LAUNCH_TICKET_SIGNING_KEYS`, `FI_SITE_KEY_PEPPER` |
//! | [`session`] | Session tokens of the game plane: `aud = game`, pool, tier, `cdb` (local environment v0, workstream A) |
//! | [`relay_grant`] | Relay grants: Ed25519 over `fi-relay-grant-v1\n` ‖ canonical claims, at most 60 s; the verify-only [`relay_grant::PublicKeys`] ring |

pub mod access;
pub mod b64;
pub mod codes;
pub mod firebase_scrypt;
pub mod idtoken;
pub mod keys;
pub mod localidp;
pub mod mcp;
pub mod opaque;
pub mod paseto;
pub mod password;
pub mod pepper;
pub mod pkce;
pub mod relay_grant;
pub mod session;
pub mod ticket;

use std::fmt;

/// A secret string (a token, a code): `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The secret, owned.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// The operating system's random source failed (nothing can be minted).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the system random source is unavailable")]
pub struct RandomUnavailable;

/// `N` bytes from the OS CSPRNG.
pub fn random_bytes<const N: usize>() -> Result<[u8; N], RandomUnavailable> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|_| RandomUnavailable)?;
    Ok(out)
}

/// SHA-256 of `bytes`: the lookup value of every stored secret.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(bytes).into()
}

/// HMAC-SHA256 of `message` under `key` (any length).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, KeyInit, Mac};
    let mut m = Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes keys of any length");
    m.update(message);
    m.finalize().into_bytes().into()
}

/// Constant-time equality of two byte strings (length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// Lower-case hex.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(format!("{:?}", Secret::new("fi_rt_x")), "Secret(***)");
        assert_eq!(hex(&[0x00, 0xab, 0x0f]), "00ab0f");
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"ab", b"abc"));
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(random_bytes::<32>().unwrap(), random_bytes::<32>().unwrap());
    }
}
