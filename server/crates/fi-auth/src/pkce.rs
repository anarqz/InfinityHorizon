//! PKCE (RFC 7636), `S256` only: the launcher keeps `code_verifier` (43-128 unreserved
//! characters), the portal's authorization page sends `code_challenge = base64url(SHA-256(
//! code_verifier))`, and the code is redeemed only with the verifier that hashes to it.

use crate::{b64, ct_eq, sha256};

/// RFC 7636 §4.1: 43 to 128 characters of `[A-Za-z0-9-._~]`.
pub fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// An `S256` challenge: 43 base64url characters of a 32-byte digest.
pub fn valid_challenge(challenge: &str) -> bool {
    challenge.len() == 43 && b64::url_decode(challenge).is_some_and(|d| d.len() == 32)
}

/// The `S256` challenge of `verifier`.
pub fn challenge_s256(verifier: &str) -> String {
    b64::url_encode(&sha256(verifier.as_bytes()))
}

/// Whether `verifier` is well formed and hashes to `challenge` (constant-time comparison).
pub fn verify_s256(verifier: &str, challenge: &str) -> bool {
    valid_verifier(verifier) && ct_eq(challenge_s256(verifier).as_bytes(), challenge.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_7636_appendix_b() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(challenge_s256(verifier), challenge);
        assert!(valid_challenge(challenge));
        assert!(verify_s256(verifier, challenge));
        assert!(!verify_s256(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXl",
            challenge
        ));
        assert!(
            !verify_s256(&verifier[..42], &challenge_s256(&verifier[..42])),
            "too short"
        );
        assert!(!valid_verifier(&"a".repeat(129)));
        assert!(!valid_verifier(&format!("{}+", "a".repeat(43))));
        assert!(!valid_challenge(
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM="
        ));
    }
}
