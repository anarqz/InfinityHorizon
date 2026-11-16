//! Passwords of a `local` instance's built-in sign-in (ACCOUNTS.md §4.1 "Two identity backends",
//! OD-122): Argon2id with the OWASP parameters (19 MiB, 2 passes, 1 lane), a 16-byte salt from
//! the OS CSPRNG, stored as its PHC string (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`).
//!
//! Both functions are CPU-heavy on purpose (tens of milliseconds): callers run them off the async
//! executor (`tokio::task::spawn_blocking`) and never while holding a database connection.
//! [`verify_unknown`] spends the same work on an e-mail that has no credential, so the time of a
//! sign-in does not reveal whether the address exists.

use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};
use std::sync::OnceLock;

/// OWASP's Argon2id memory cost (KiB).
pub const M_COST_KIB: u32 = 19_456;
/// OWASP's Argon2id passes.
pub const T_COST: u32 = 2;
/// OWASP's Argon2id lanes.
pub const P_COST: u32 = 1;
/// Shortest password (characters).
pub const MIN_CHARS: usize = 8;
/// Longest password (characters).
pub const MAX_CHARS: usize = 128;

/// A password could not be hashed (the random source failed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the password could not be hashed")]
pub struct HashError;

fn hasher() -> Argon2<'static> {
    // The parameters are constants inside Argon2's bounds: `new` cannot fail on them.
    let params = Params::new(M_COST_KIB, T_COST, P_COST, Some(32)).unwrap_or_default();
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Whether `password` has an accepted length (8-128 characters).
pub fn acceptable(password: &str) -> bool {
    (MIN_CHARS..=MAX_CHARS).contains(&password.chars().count())
}

/// The PHC string of `password` under a fresh random salt.
pub fn hash(password: &str) -> Result<String, HashError> {
    let salt = crate::random_bytes::<16>().map_err(|_| HashError)?;
    hasher()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| HashError)
}

/// Whether `password` matches the PHC string `phc` (the hash is compared in constant time; a
/// malformed or non-Argon2 string never matches).
pub fn verify(password: &str, phc: &str) -> bool {
    if !phc.starts_with("$argon2id$") {
        return false;
    }
    hasher().verify_password(password.as_bytes(), phc).is_ok()
}

/// The same work as [`verify`] for an e-mail without a credential; always `false`.
pub fn verify_unknown(password: &str) -> bool {
    static DUMMY: OnceLock<Option<String>> = OnceLock::new();
    let dummy = DUMMY.get_or_init(|| hash("fi-local-idp-dummy-password").ok());
    if let Some(dummy) = dummy {
        let _ = verify(password, dummy);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_verify_and_never_repeat() {
        let a = hash("synthetic-pass-1").unwrap();
        let b = hash("synthetic-pass-1").unwrap();
        assert!(a.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"), "{a}");
        assert_ne!(a, b, "a fresh salt each time");
        assert!(verify("synthetic-pass-1", &a) && verify("synthetic-pass-1", &b));
        assert!(!verify("synthetic-pass-2", &a));
        assert!(!verify("", &a));
        assert!(!a.contains("synthetic"));
    }

    #[test]
    fn malformed_or_other_hashes_never_match() {
        let good = hash("synthetic-pass-1").unwrap();
        let swap = if good.ends_with('A') { 'B' } else { 'A' };
        let tampered = format!("{}{swap}", &good[..good.len() - 1]);
        for phc in [
            "",
            "synthetic-pass-1",
            "$argon2i$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaA",
            tampered.as_str(),
        ] {
            assert!(!verify("synthetic-pass-1", phc), "{phc}");
        }
        assert!(!verify_unknown("synthetic-pass-1"));
    }

    #[test]
    fn lengths() {
        assert!(!acceptable("1234567"));
        assert!(acceptable("12345678"));
        assert!(acceptable(&"é".repeat(128)));
        assert!(!acceptable(&"x".repeat(129)));
    }
}
