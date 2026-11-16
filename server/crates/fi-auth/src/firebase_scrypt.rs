//! Firebase Authentication's modified scrypt, for the users a `local` instance imports from a
//! Firebase project (OD-122 follow-up 2026-10-26 "the owner's answers to every open question":
//! "the Firebase scrypt hash is imported for password users and rehashed to Argon2id at first
//! sign-in"; ACCOUNTS.md §4.1 "Firebase import").
//!
//! The algorithm, as published by Firebase in <https://github.com/firebase/scrypt> (README
//! "Password Hashing") and the community ports it links (firebase-scrypt-python):
//!
//! 1. `salt = base64(user salt) ‖ base64(project salt separator)` (each decoded first);
//! 2. `key = scrypt(password, salt, N = 2^mem_cost, r = rounds, p = 1)`, first 32 bytes;
//! 3. `hash = AES-256-CTR(key, IV = 16 zero bytes)` applied to the decoded project signer key;
//! 4. the user's `passwordHash` is `base64(hash)`.
//!
//! The project's hash parameters (`base64_signer_key`, `base64_salt_separator`, `rounds`,
//! `mem_cost`) come from the Firebase console's "Password Hash Parameters" and are configuration
//! (`FI_FIREBASE_SCRYPT_*`), never stored with a user: the signer key lets anyone who holds it test
//! guesses offline. A stored imported hash is `$firebase-scrypt$<salt b64>$<hash b64>`.
//!
//! CPU- and memory-heavy (N = 2^14, r = 8 is 16 MiB): callers run it where they run Argon2id.

use crate::{b64, ct_eq};
use aes::cipher::{KeyIvInit, StreamCipher};
use std::fmt;

/// The stored form's prefix.
pub const PREFIX: &str = "$firebase-scrypt$";
/// Largest `mem_cost` accepted (2^15 blocks: at most 64 MiB with 16 rounds).
pub const MAX_MEM_COST: u8 = 15;
/// Largest `rounds` accepted.
pub const MAX_ROUNDS: u32 = 16;

type Aes256Ctr = ctr::Ctr128BE<aes::Aes256>;

/// A Firebase project's password hash parameters.
#[derive(Clone, PartialEq, Eq)]
pub struct HashConfig {
    signer_key: Vec<u8>,
    salt_separator: Vec<u8>,
    rounds: u32,
    mem_cost: u8,
}

impl fmt::Debug for HashConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HashConfig")
            .field("rounds", &self.rounds)
            .field("mem_cost", &self.mem_cost)
            .finish_non_exhaustive()
    }
}

/// A malformed hash parameter (the reason names the field, never its value).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub &'static str);

impl HashConfig {
    /// The parameters as the Firebase console prints them (base64 key and separator).
    pub fn new(
        base64_signer_key: &str,
        base64_salt_separator: &str,
        rounds: u32,
        mem_cost: u8,
    ) -> Result<Self, ConfigError> {
        let signer_key = b64::std_decode(base64_signer_key.trim())
            .filter(|k| !k.is_empty())
            .ok_or(ConfigError("the signer key is not base64"))?;
        let salt_separator = b64::std_decode(base64_salt_separator.trim())
            .ok_or(ConfigError("the salt separator is not base64"))?;
        if !(1..=MAX_ROUNDS).contains(&rounds) {
            return Err(ConfigError("rounds: 1 to 16"));
        }
        if !(1..=MAX_MEM_COST).contains(&mem_cost) {
            return Err(ConfigError("mem_cost: 1 to 15"));
        }
        Ok(Self {
            signer_key,
            salt_separator,
            rounds,
            mem_cost,
        })
    }

    /// Firebase's hash of `password` under the user's decoded `salt`.
    pub fn hash(&self, password: &[u8], salt: &[u8]) -> Option<Vec<u8>> {
        let mut full_salt = Vec::with_capacity(salt.len() + self.salt_separator.len());
        full_salt.extend_from_slice(salt);
        full_salt.extend_from_slice(&self.salt_separator);
        let params = scrypt::Params::new(self.mem_cost, self.rounds, 1).ok()?;
        let mut key = [0u8; 32];
        scrypt::scrypt(password, &full_salt, &params, &mut key).ok()?;
        let mut out = self.signer_key.clone();
        let mut cipher = Aes256Ctr::new(&key.into(), &[0u8; 16].into());
        cipher.apply_keystream(&mut out);
        Some(out)
    }

    /// Whether `password` matches a stored `$firebase-scrypt$<salt>$<hash>` (constant-time
    /// comparison; a malformed string never matches).
    pub fn verify(&self, password: &str, stored: &str) -> bool {
        let Some((salt, hash)) = parse(stored) else {
            return false;
        };
        self.hash(password.as_bytes(), &salt)
            .is_some_and(|computed| ct_eq(&computed, &hash))
    }
}

/// The stored form of an exported user's `salt` and `passwordHash` (both base64), or `None` when
/// either is not base64 or the hash is empty.
pub fn stored(base64_salt: &str, base64_hash: &str) -> Option<String> {
    let salt = b64::std_decode(base64_salt.trim())?;
    let hash = b64::std_decode(base64_hash.trim()).filter(|h| !h.is_empty())?;
    Some(format!(
        "{PREFIX}{}${}",
        b64::std_encode(&salt),
        b64::std_encode(&hash)
    ))
}

/// Whether `stored` is an imported Firebase hash.
pub fn is_firebase(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

fn parse(stored: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let (salt, hash) = stored.strip_prefix(PREFIX)?.split_once('$')?;
    Some((b64::std_decode(salt)?, b64::std_decode(hash)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The known answer published by Firebase in https://github.com/firebase/scrypt (README,
    // "Password Hashing": the sample project parameters, the exported user `user1@test.com` and
    // its password `user1password`, "Expected output: lSrfV15c…"). Public documentation values,
    // not a secret of any project.
    const SIGNER_KEY: &str =
        "jxspr8Ki0RYycVU8zykbdLGjFQ3McFUH0uiiTvC8pVMXAn210wjLNmdZJzxUECKbm0QsEmYUSDzZvpjeJ9WmXA==";
    const SALT_SEPARATOR: &str = "Bw==";
    const ROUNDS: u32 = 8;
    const MEM_COST: u8 = 14;
    const USER_SALT: &str = "42xEC+ixf3L2lw==";
    const USER_HASH: &str =
        "lSrfV15cpx95/sZS2W9c9Kp6i/LVgQNDNC/qzrCnh1SAyZvqmZqAjTdn3aoItz+VHjoZilo78198JAdRuid5lQ==";

    fn config() -> HashConfig {
        HashConfig::new(SIGNER_KEY, SALT_SEPARATOR, ROUNDS, MEM_COST).unwrap()
    }

    #[test]
    fn firebase_published_known_answer() {
        let c = config();
        let salt = b64::std_decode(USER_SALT).unwrap();
        let hash = c.hash(b"user1password", &salt).unwrap();
        assert_eq!(b64::std_encode(&hash), USER_HASH);
        let s = stored(USER_SALT, USER_HASH).unwrap();
        assert!(is_firebase(&s));
        assert!(c.verify("user1password", &s));
        assert!(!c.verify("user1password ", &s));
        assert!(!c.verify("user2password", &s));
    }

    #[test]
    fn malformed_inputs_never_match_and_bad_parameters_are_refused() {
        let c = config();
        for s in [
            "",
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA",
            "$firebase-scrypt$",
            "$firebase-scrypt$not base64$x",
            "$firebase-scrypt$42xEC+ixf3L2lw==",
        ] {
            assert!(!c.verify("user1password", s), "{s}");
        }
        assert_eq!(stored(USER_SALT, ""), None);
        assert_eq!(stored("%%", USER_HASH), None);
        assert!(HashConfig::new("", SALT_SEPARATOR, 8, 14).is_err());
        assert!(HashConfig::new(SIGNER_KEY, "%%", 8, 14).is_err());
        assert!(HashConfig::new(SIGNER_KEY, SALT_SEPARATOR, 0, 14).is_err());
        assert!(HashConfig::new(SIGNER_KEY, SALT_SEPARATOR, 8, 16).is_err());
        assert!(!format!("{c:?}").contains("jxspr8"));
    }
}
