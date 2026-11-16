//! Gamertags. Since OD-120 (ACCOUNTS.md §3) a gamertag is the plain `Name`, unique per game
//! without regard to case; [`GamertagInput`] is what players type or clients send.
//!
//! The name rule is carried over from Beta (`server/src/db/accounts.rs:94-106`): 1 to 20 ASCII
//! letters or digits, starting with a letter. [`Gamertag`] is the **legacy** `Name#12345` (a
//! 5-digit discriminator), still accepted as input during the transition and never emitted; the
//! portal's legacy URL form was `Name-12345`.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Longest name (the game shows 15 characters; Beta cuts the name to keep the number visible).
pub const MAX_NAME: usize = 20;
/// Discriminators are 00000-99999.
pub const MAX_DISCRIMINATOR: u32 = 99_999;

/// A validated legacy gamertag `Name#12345` (input only since OD-120).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Gamertag {
    name: String,
    discriminator: u32,
}

/// Why a gamertag was refused. The codes are what the account API returns.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GamertagError {
    /// Empty, too long, not starting with a letter, or with a character that is not a letter or digit.
    #[error("a name is 1 to 20 letters or digits and starts with a letter")]
    InvalidName,
    /// Not exactly 5 digits.
    #[error("the number after # is exactly 5 digits")]
    InvalidDiscriminator,
    /// No `#` (or `-` in the URL form) between name and number.
    #[error("a gamertag looks like Name#12345")]
    Malformed,
}

impl GamertagError {
    /// Stable error code (docs/contracts/ACCOUNTS.md §9).
    pub const fn code(&self) -> &'static str {
        match self {
            GamertagError::InvalidName => "ACCOUNTS.INVALID_NAME",
            GamertagError::InvalidDiscriminator | GamertagError::Malformed => {
                "ACCOUNTS.INVALID_GAMERTAG"
            }
        }
    }
}

/// Names nobody may take (OD-120 follow-up): the whole name, compared case-insensitively. Existing
/// profiles keep theirs; new names and renames are refused like a taken one.
pub const RESERVED_NAMES: [&str; 12] = [
    "admin",
    "administrator",
    "moderator",
    "mod",
    "staff",
    "support",
    "system",
    "infinity",
    "forzainfinity",
    "infinityhorizon",
    "official",
    "null",
];

/// Whether `name` is reserved ([`RESERVED_NAMES`]).
pub fn is_reserved(name: &str) -> bool {
    RESERVED_NAMES
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

/// A gamertag as typed or sent: the plain name, or the legacy `Name#12345` (transition only).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GamertagInput {
    /// `Name`, a valid name.
    Plain(String),
    /// `Name#12345`, resolved through the profile's legacy gamertag.
    Legacy(Gamertag),
}

impl GamertagInput {
    /// The name part, as typed.
    pub fn name(&self) -> &str {
        match self {
            GamertagInput::Plain(name) => name,
            GamertagInput::Legacy(tag) => tag.name(),
        }
    }

    /// Parses a handle: the plain name, or the legacy URL form `Name-12345`.
    pub fn from_handle(handle: &str) -> Result<Self, GamertagError> {
        let handle = handle.trim();
        if valid_name(handle) {
            return Ok(GamertagInput::Plain(handle.to_owned()));
        }
        Gamertag::from_handle(handle).map(GamertagInput::Legacy)
    }
}

impl FromStr for GamertagInput {
    type Err = GamertagError;

    fn from_str(s: &str) -> Result<Self, GamertagError> {
        let s = s.trim();
        if s.contains('#') {
            return s.parse().map(GamertagInput::Legacy);
        }
        if valid_name(s) {
            Ok(GamertagInput::Plain(s.to_owned()))
        } else {
            Err(GamertagError::InvalidName)
        }
    }
}

/// Whether `name` is a valid profile name.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic())
        && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

impl Gamertag {
    /// A gamertag from its parts.
    pub fn new(name: &str, discriminator: u32) -> Result<Self, GamertagError> {
        if !valid_name(name) {
            return Err(GamertagError::InvalidName);
        }
        if discriminator > MAX_DISCRIMINATOR {
            return Err(GamertagError::InvalidDiscriminator);
        }
        Ok(Self {
            name: name.to_owned(),
            discriminator,
        })
    }

    /// Parses the URL form `Name-12345`.
    pub fn from_handle(handle: &str) -> Result<Self, GamertagError> {
        let (name, number) = handle.rsplit_once('-').ok_or(GamertagError::Malformed)?;
        Self::from_parts(name, number)
    }

    fn from_parts(name: &str, number: &str) -> Result<Self, GamertagError> {
        if number.len() != 5 || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(GamertagError::InvalidDiscriminator);
        }
        let discriminator = number
            .parse()
            .map_err(|_| GamertagError::InvalidDiscriminator)?;
        Self::new(name, discriminator)
    }

    /// The name as the player typed it (case kept for display).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The 5-digit number.
    pub const fn discriminator(&self) -> u32 {
        self.discriminator
    }

    /// Case-folded name: what uniqueness and look-ups compare.
    pub fn name_key(&self) -> String {
        self.name.to_ascii_lowercase()
    }

    /// The portal URL form, `Name-12345`.
    pub fn handle(&self) -> String {
        format!("{}-{:05}", self.name, self.discriminator)
    }
}

impl FromStr for Gamertag {
    type Err = GamertagError;

    fn from_str(s: &str) -> Result<Self, GamertagError> {
        let (name, number) = s.trim().rsplit_once('#').ok_or(GamertagError::Malformed)?;
        Self::from_parts(name, number)
    }
}

impl TryFrom<String> for Gamertag {
    type Error = GamertagError;

    fn try_from(value: String) -> Result<Self, GamertagError> {
        value.parse()
    }
}

impl From<Gamertag> for String {
    fn from(value: Gamertag) -> String {
        value.to_string()
    }
}

impl fmt::Display for Gamertag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{:05}", self.name, self.discriminator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats() {
        let tag: Gamertag = "Speedy42#00731".parse().unwrap();
        assert_eq!(tag.name(), "Speedy42");
        assert_eq!(tag.discriminator(), 731);
        assert_eq!(tag.to_string(), "Speedy42#00731");
        assert_eq!(tag.handle(), "Speedy42-00731");
        assert_eq!(Gamertag::from_handle("Speedy42-00731").unwrap(), tag);
        assert_eq!(tag.name_key(), "speedy42");
    }

    #[test]
    fn beta_name_rules() {
        assert!(valid_name("A"));
        assert!(valid_name("Abcdefghij0123456789"));
        assert!(!valid_name("Abcdefghij01234567890"), "21 characters");
        assert!(!valid_name("9lives"), "starts with a digit");
        assert!(!valid_name("bad name"));
        assert!(!valid_name("Zoë"));
        assert!(!valid_name(""));
    }

    #[test]
    fn refuses_malformed_tags() {
        assert_eq!(
            "NoNumber".parse::<Gamertag>(),
            Err(GamertagError::Malformed)
        );
        assert_eq!(
            "Name#1234".parse::<Gamertag>(),
            Err(GamertagError::InvalidDiscriminator)
        );
        assert_eq!(
            "Name#123456".parse::<Gamertag>(),
            Err(GamertagError::InvalidDiscriminator)
        );
        assert_eq!(
            "Name#12a45".parse::<Gamertag>(),
            Err(GamertagError::InvalidDiscriminator)
        );
        assert_eq!(
            "#12345".parse::<Gamertag>(),
            Err(GamertagError::InvalidName)
        );
        assert_eq!(
            Gamertag::new("Ok", 100_000),
            Err(GamertagError::InvalidDiscriminator)
        );
    }

    #[test]
    fn serde_uses_the_display_form() {
        let tag = Gamertag::new("Drift", 5).unwrap();
        assert_eq!(serde_json::to_string(&tag).unwrap(), "\"Drift#00005\"");
        assert!(serde_json::from_str::<Gamertag>("\"Drift#5\"").is_err());
    }

    #[test]
    fn input_takes_plain_names_and_legacy_tags() {
        assert_eq!(
            "Speedy42".parse::<GamertagInput>(),
            Ok(GamertagInput::Plain("Speedy42".into()))
        );
        assert_eq!(
            " Speedy42#00731 ".parse::<GamertagInput>(),
            Ok(GamertagInput::Legacy(
                Gamertag::new("Speedy42", 731).unwrap()
            ))
        );
        assert_eq!(
            "Speedy42".parse::<GamertagInput>().unwrap().name(),
            "Speedy42"
        );
        assert_eq!(
            "9lives".parse::<GamertagInput>(),
            Err(GamertagError::InvalidName)
        );
        assert_eq!(
            "Name#1234".parse::<GamertagInput>(),
            Err(GamertagError::InvalidDiscriminator)
        );
        assert_eq!(
            GamertagInput::from_handle("Speedy42"),
            Ok(GamertagInput::Plain("Speedy42".into()))
        );
        assert_eq!(
            GamertagInput::from_handle("Speedy42-00731"),
            Ok(GamertagInput::Legacy(
                Gamertag::new("Speedy42", 731).unwrap()
            ))
        );
    }

    #[test]
    fn reserved_names_are_whole_and_case_insensitive() {
        assert!(is_reserved("Admin"));
        assert!(is_reserved("FORZAINFINITY"));
        assert!(!is_reserved("Admin2"));
        assert!(!is_reserved("Moderators"));
        assert!(!is_reserved("Speedy42"));
    }
}
