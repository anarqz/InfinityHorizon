//! Site keys: the `K7QM-4XPA-9RT2` secret that, with a gamertag, signs a player in to the portal
//! and the launcher (ACCOUNTS.md §5). Format kept from Beta (`server/src/db/accounts.rs:43-58`) so
//! keys players already wrote down keep working after the migration.
//!
//! 12 characters from a 32-letter alphabet without 0/O or 1/I (readable off a TV): 60 bits. The
//! key is random, not chosen by a person, so the server stores a keyed hash (HMAC-SHA256 with a
//! pepper held in Secret Manager) rather than a slow password hash; this crate only handles the
//! format. Generation draws from the OS CSPRNG.

use std::fmt;
use std::str::FromStr;

/// Characters a key is made of (Beta `WEB_KEY_ALPHABET`).
pub const ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
/// Characters in a key, without the dashes.
pub const LEN: usize = 12;

/// A site key in canonical form (12 upper-case characters, no dashes). `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct SiteKey([u8; LEN]);

/// Why a typed key cannot be a site key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SiteKeyError {
    /// Wrong length or a character outside the alphabet once dashes and spaces are removed.
    #[error("a site key is 12 letters and digits, like K7QM-4XPA-9RT2")]
    Malformed,
    /// The operating system's random source failed.
    #[error("the system random source is unavailable")]
    Random,
}

impl SiteKey {
    /// A fresh random key.
    pub fn generate() -> Result<Self, SiteKeyError> {
        let mut bytes = [0u8; LEN];
        getrandom::fill(&mut bytes).map_err(|_| SiteKeyError::Random)?;
        // 256 is a multiple of 32, so masking keeps every character equally likely.
        Ok(Self(bytes.map(|b| ALPHABET[(b & 31) as usize])))
    }

    /// The canonical 12 characters: what the server hashes and compares.
    pub fn canonical(&self) -> &str {
        // Only ASCII from ALPHABET is ever stored.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

impl FromStr for SiteKey {
    type Err = SiteKeyError;

    /// Accepts what a player types: any case, with or without dashes and spaces (Beta
    /// `web_key_canonical`). Ambiguous characters are refused, not guessed.
    fn from_str(s: &str) -> Result<Self, SiteKeyError> {
        let mut out = [0u8; LEN];
        let mut n = 0;
        for c in s.chars().filter(|c| !matches!(c, '-' | ' ')) {
            let upper = c.to_ascii_uppercase();
            if n == LEN || !upper.is_ascii() || !ALPHABET.contains(&(upper as u8)) {
                return Err(SiteKeyError::Malformed);
            }
            out[n] = upper as u8;
            n += 1;
        }
        if n != LEN {
            return Err(SiteKeyError::Malformed);
        }
        Ok(Self(out))
    }
}

impl fmt::Display for SiteKey {
    /// The display form `XXXX-XXXX-XXXX`, shown to the player exactly once per issue.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.canonical();
        write!(f, "{}-{}-{}", &s[..4], &s[4..8], &s[8..])
    }
}

impl fmt::Debug for SiteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SiteKey(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_use_the_alphabet() {
        for _ in 0..200 {
            let key = SiteKey::generate().unwrap();
            assert_eq!(key.canonical().len(), LEN);
            assert!(key.canonical().bytes().all(|b| ALPHABET.contains(&b)));
            let shown = key.to_string();
            assert_eq!(shown.len(), 14);
            assert_eq!(shown.parse::<SiteKey>().unwrap(), key);
        }
    }

    #[test]
    fn typed_forms_canonicalize() {
        let key: SiteKey = "k7qm 4xpa-9rt2".parse().unwrap();
        assert_eq!(key.canonical(), "K7QM4XPA9RT2");
        assert_eq!(key.to_string(), "K7QM-4XPA-9RT2");
    }

    #[test]
    fn ambiguous_or_wrong_lengths_are_refused() {
        assert_eq!(
            "K7QM-4XPA-9RT".parse::<SiteKey>(),
            Err(SiteKeyError::Malformed)
        );
        assert_eq!(
            "K7QM-4XPA-9RT22".parse::<SiteKey>(),
            Err(SiteKeyError::Malformed)
        );
        assert_eq!(
            "K7QM-4XPA-9RT0".parse::<SiteKey>(),
            Err(SiteKeyError::Malformed),
            "0 is not in the alphabet"
        );
        assert_eq!(
            "K7QM-4XPA-9RTO".parse::<SiteKey>(),
            Err(SiteKeyError::Malformed),
            "O is not either"
        );
        assert_eq!(
            "K7QM-4XPA-9RTé".parse::<SiteKey>(),
            Err(SiteKeyError::Malformed)
        );
    }

    #[test]
    fn debug_hides_the_key() {
        let key: SiteKey = "K7QM-4XPA-9RT2".parse().unwrap();
        assert_eq!(format!("{key:?}"), "SiteKey(***)");
    }
}
