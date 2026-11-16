//! The fresh-start check (CR-S51; CLOUD_SAVES.md §7.4, SAVES_API.md §4.9): is a Hardcore line's
//! first commit (`fresh_start`) a career Azimuth has just created? The owner (2026-10-26): "Zero
//! for everything then it starts receiving stuff right after the first intro race", so every
//! ceiling defaults to 0; a server may loosen one ([`FreshStartLimits`], `FI_FRESH_START_MAX_*`)
//! if Azimuth changes its first save. The PlayerDatabase facts ([`PlayerDbFacts`]) are read by the
//! server (`fi-playerdb`, which this crate does not depend on: it builds into the launcher) and
//! handed in; an unreadable database fails closed (`PLAYERDB_UNREADABLE`).
//!
//! Pure: no I/O. The caller has already refused unreadable credits, XP or level
//! (`SAVES.SUMMARY_UNREADABLE`); a number the save lacks other than those (winnings, tokens) is not
//! judged.

use crate::model::SaveSummary;

/// The `rule` of a fresh-start refusal (`SAVES.GATE_REFUSED.rule`).
pub const NOT_FRESH_START: &str = "NOT_FRESH_START";

/// The fresh-start ceilings (server configuration; all 0 by default, the owner's "zero for
/// everything") and which numbers a first save may lack (none by default).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreshStartLimits {
    /// `FI_FRESH_START_MAX_CREDITS`: ProfileData `Main/Credits`.
    pub credits: i64,
    /// `FI_FRESH_START_MAX_WINNINGS`: `Main/TotalWinnings`.
    pub winnings: i64,
    /// `FI_FRESH_START_MAX_XP`: `Main/XP`.
    pub xp: i64,
    /// `FI_FRESH_START_MAX_LEVEL`: the driver level `Main/XP` reaches (the server derives it,
    /// `gates::with_driver_level`; ProfileData `Main/Level` is not the driver level).
    pub level: i64,
    /// `FI_FRESH_START_MAX_CARS`: `Career_Garage` rows.
    pub cars: i64,
    /// `FI_FRESH_START_MAX_TOKENS`: `Main/NumSkillTokens`.
    pub tokens: i64,
    /// `FI_FRESH_START_ALLOW_MISSING` lists `winnings`: a first save without a readable
    /// `Main/TotalWinnings` is accepted (the line's winnings conservation then has no base).
    pub allow_missing_winnings: bool,
    /// `FI_FRESH_START_ALLOW_MISSING` lists `tokens`: a first save without a readable skill state
    /// (`Main/NumSkillTokens`, SaveState `SkillPointsTotal`, `UnspentSkillPointsTotal`) is accepted
    /// (the line's token schedule then has no base).
    pub allow_missing_tokens: bool,
}

/// A `FI_FRESH_START_ALLOW_MISSING` entry other than `winnings` or `tokens`.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown fresh-start allowance {0:?}: use winnings or tokens")]
pub struct UnknownAllowance(pub String);

impl FreshStartLimits {
    /// Applies `FI_FRESH_START_ALLOW_MISSING` (a comma list of `winnings`, `tokens`; empty: none).
    pub fn allowing_missing(mut self, text: &str) -> Result<Self, UnknownAllowance> {
        for word in text.split(',').map(str::trim).filter(|w| !w.is_empty()) {
            match word {
                "winnings" => self.allow_missing_winnings = true,
                "tokens" => self.allow_missing_tokens = true,
                other => return Err(UnknownAllowance(other.to_owned())),
            }
        }
        Ok(self)
    }
}

/// What the server read of the commit's PlayerDatabase (`fi-playerdb`), as counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlayerDbFacts {
    /// `Career_Garage` rows.
    pub cars: i64,
    /// `PerkUnlock` rows.
    pub perks: i64,
    /// `FreeCars` rows (vouchers).
    pub vouchers: i64,
    /// `Career_EventCarsClaimed` rows.
    pub event_cars: i64,
    /// `Career_PurchasedParts` rows with `PricePaid` > 0.
    pub parts_paid: i64,
    /// `BarnFinds` rows with `State` ≠ 0.
    pub barn_finds: i64,
}

/// The bundle's PlayerDatabase did not read (the check fails closed: `PLAYERDB_UNREADABLE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerDbUnreadable;

/// Reads a bundle's PlayerDatabase facts (the server's `fi-playerdb` reader).
pub type FactsReader = fn(&[u8]) -> Result<PlayerDbFacts, PlayerDbUnreadable>;

/// How a `fresh_start` commit is checked: the server's ceilings and its PlayerDatabase reader.
#[derive(Clone, Copy, Debug)]
pub struct FreshStartCheck {
    /// The ceilings.
    pub limits: FreshStartLimits,
    /// Reads the bundle's PlayerDatabase.
    pub facts: FactsReader,
}

/// The reasons a save is not a fresh career, in the contract's order (`FRESH_*`,
/// `PLAYERDB_UNREADABLE`); empty when it is one. Every number must be readable and within
/// `0..=ceiling` (a negative number is its own reason too: it would poison the line's base);
/// winnings and the skill state (tokens, skill points, unspent points) must be readable unless
/// the server allows them missing (`FRESH_FIELD_MISSING`), so the line's anchor always carries the
/// numbers its conservation rules measure from.
pub fn check(
    limits: &FreshStartLimits,
    summary: &SaveSummary,
    facts: Result<&PlayerDbFacts, PlayerDbUnreadable>,
) -> Vec<&'static str> {
    let mut out = Vec::new();
    let outside = |value: i64, ceiling: i64| !(0..=ceiling).contains(&value);
    // Credits, XP and level are readable here (the caller refused otherwise); a missing one is
    // judged outside its ceiling all the same (fail closed).
    let required = |value: Option<i64>, ceiling: i64| value.is_none_or(|v| outside(v, ceiling));
    let optional = |value: Option<i64>, ceiling: i64| value.is_some_and(|v| outside(v, ceiling));
    let mut missing = false;
    if required(summary.credits, limits.credits) {
        out.push("FRESH_CREDITS");
    }
    if optional(summary.winnings, limits.winnings) {
        out.push("FRESH_WINNINGS");
    }
    missing |= summary.winnings.is_none() && !limits.allow_missing_winnings;
    if required(summary.xp, limits.xp) {
        out.push("FRESH_XP");
    }
    if required(summary.level, limits.level) {
        out.push("FRESH_LEVEL");
    }
    let cars = match facts {
        Ok(f) => outside(f.cars, limits.cars),
        Err(PlayerDbUnreadable) => false,
    };
    if cars {
        out.push("FRESH_CARS");
    }
    let skills_negative = [summary.skill_points, summary.unspent_skill_points]
        .iter()
        .any(|v| v.is_some_and(|v| v < 0));
    if optional(summary.tokens, limits.tokens) || skills_negative {
        out.push("FRESH_TOKENS");
    }
    let skills = [
        summary.tokens,
        summary.skill_points,
        summary.unspent_skill_points,
    ];
    missing |= skills.iter().any(Option::is_none) && !limits.allow_missing_tokens;
    match facts {
        Ok(f) => {
            for (count, reason) in [
                (f.perks, "FRESH_PERKS"),
                (f.vouchers, "FRESH_VOUCHERS"),
                (f.event_cars, "FRESH_EVENT_CARS"),
                (f.parts_paid, "FRESH_PARTS_PAID"),
                (f.barn_finds, "FRESH_BARN_FINDS"),
            ] {
                if count > 0 {
                    out.push(reason);
                }
            }
        }
        Err(PlayerDbUnreadable) => out.push("PLAYERDB_UNREADABLE"),
    }
    if missing {
        out.push("FRESH_FIELD_MISSING");
    }
    out
}

/// A reader for builds without the server's PlayerDatabase reader: every database is unreadable,
/// so the check fails closed.
pub fn unreadable(_: &[u8]) -> Result<PlayerDbFacts, PlayerDbUnreadable> {
    Err(PlayerDbUnreadable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero() -> SaveSummary {
        SaveSummary {
            credits: Some(0),
            xp: Some(0),
            level: Some(0),
            winnings: Some(0),
            tokens: Some(0),
            skill_points: Some(0),
            unspent_skill_points: Some(0),
            ..SaveSummary::default()
        }
    }

    #[test]
    fn a_new_career_at_zero_passes_the_defaults() {
        let limits = FreshStartLimits::default();
        assert!(check(&limits, &zero(), Ok(&PlayerDbFacts::default())).is_empty());
    }

    /// Review M1: a first save without winnings or its skill state would anchor the line with
    /// those rules off for good: refused unless the server allows the field missing.
    #[test]
    fn a_missing_winnings_or_skill_state_is_refused_unless_allowed() {
        let none = Ok(&PlayerDbFacts::default());
        let strict = FreshStartLimits::default();
        for lacks in [
            SaveSummary {
                winnings: None,
                ..zero()
            },
            SaveSummary {
                tokens: None,
                ..zero()
            },
            SaveSummary {
                unspent_skill_points: None,
                ..zero()
            },
        ] {
            assert_eq!(check(&strict, &lacks, none), ["FRESH_FIELD_MISSING"]);
        }
        let both = SaveSummary {
            winnings: None,
            tokens: None,
            skill_points: None,
            unspent_skill_points: None,
            ..zero()
        };
        let allowed = strict.allowing_missing(" winnings, tokens ").unwrap();
        assert!(check(&allowed, &both, none).is_empty());
        let only_winnings = strict.allowing_missing("winnings").unwrap();
        assert_eq!(check(&only_winnings, &both, none), ["FRESH_FIELD_MISSING"]);
        assert!(strict.allowing_missing("credits").is_err());
        assert_eq!(strict.allowing_missing("").unwrap(), strict);
    }

    /// Review M2: a negative number never anchors a line.
    #[test]
    fn negative_numbers_are_refused() {
        let none = Ok(&PlayerDbFacts::default());
        let limits = FreshStartLimits::default();
        let neg = SaveSummary {
            credits: Some(-1),
            xp: Some(-1),
            level: Some(-1),
            winnings: Some(-10_000_000),
            tokens: Some(-1),
            ..zero()
        };
        assert_eq!(
            check(&limits, &neg, none),
            [
                "FRESH_CREDITS",
                "FRESH_WINNINGS",
                "FRESH_XP",
                "FRESH_LEVEL",
                "FRESH_TOKENS"
            ]
        );
        let neg_points = SaveSummary {
            skill_points: Some(-5),
            ..zero()
        };
        assert_eq!(check(&limits, &neg_points, none), ["FRESH_TOKENS"]);
        let neg_cars = PlayerDbFacts {
            cars: -1,
            ..PlayerDbFacts::default()
        };
        assert_eq!(check(&limits, &zero(), Ok(&neg_cars)), ["FRESH_CARS"]);
    }

    #[test]
    fn every_nonzero_number_is_its_reason() {
        let limits = FreshStartLimits::default();
        let s = SaveSummary {
            credits: Some(1),
            xp: Some(1),
            level: Some(1),
            winnings: Some(1),
            tokens: Some(1),
            ..zero()
        };
        let f = PlayerDbFacts {
            cars: 1,
            perks: 1,
            vouchers: 1,
            event_cars: 1,
            parts_paid: 1,
            barn_finds: 1,
        };
        assert_eq!(
            check(&limits, &s, Ok(&f)),
            [
                "FRESH_CREDITS",
                "FRESH_WINNINGS",
                "FRESH_XP",
                "FRESH_LEVEL",
                "FRESH_CARS",
                "FRESH_TOKENS",
                "FRESH_PERKS",
                "FRESH_VOUCHERS",
                "FRESH_EVENT_CARS",
                "FRESH_PARTS_PAID",
                "FRESH_BARN_FINDS",
            ]
        );
        assert_eq!(
            check(&limits, &zero(), Err(PlayerDbUnreadable)),
            ["PLAYERDB_UNREADABLE"],
            "fail closed"
        );
    }

    #[test]
    fn a_server_may_loosen_a_ceiling() {
        let limits = FreshStartLimits {
            credits: 10_000,
            cars: 1,
            ..FreshStartLimits::default()
        };
        let s = SaveSummary {
            credits: Some(10_000),
            ..zero()
        };
        let f = PlayerDbFacts {
            cars: 1,
            ..PlayerDbFacts::default()
        };
        assert!(check(&limits, &s, Ok(&f)).is_empty());
        let more = SaveSummary {
            credits: Some(10_001),
            ..zero()
        };
        assert_eq!(check(&limits, &more, Ok(&f)), ["FRESH_CREDITS"]);
        assert_eq!(unreadable(b""), Err(PlayerDbUnreadable));
    }
}

/// `FI_HARDCORE_START` (SAVES_API.md §4.9): how a Hardcore line at revision 0 starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HardcoreStart {
    /// Unset (the default): `fresh_start` only when the game has no active seed, so a server with a
    /// seed (production) is unchanged.
    #[default]
    Auto,
    /// `seed`: always the seed (`POST .../save/seed`; without one new lines cannot start).
    Seed,
    /// `fresh_start`: always Azimuth's first save.
    FreshStart,
}

impl HardcoreStart {
    /// Parses `seed` or `fresh_start` (unset is [`HardcoreStart::Auto`], decided by the caller).
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "seed" => Some(Self::Seed),
            "fresh_start" => Some(Self::FreshStart),
            _ => None,
        }
    }

    /// Whether a Hardcore line at revision 0 starts fresh, given whether its game has an active
    /// seed.
    pub const fn starts_fresh(self, active_seed: bool) -> bool {
        match self {
            Self::Auto => !active_seed,
            Self::Seed => false,
            Self::FreshStart => true,
        }
    }
}

#[cfg(test)]
mod start_tests {
    use super::HardcoreStart;

    #[test]
    fn production_with_a_seed_and_no_setting_keeps_the_seed() {
        assert!(!HardcoreStart::default().starts_fresh(true));
        assert!(HardcoreStart::default().starts_fresh(false));
        assert!(!HardcoreStart::Seed.starts_fresh(false));
        assert!(HardcoreStart::FreshStart.starts_fresh(true));
        assert_eq!(HardcoreStart::parse("seed"), Some(HardcoreStart::Seed));
        assert_eq!(
            HardcoreStart::parse("fresh_start"),
            Some(HardcoreStart::FreshStart)
        );
        assert_eq!(HardcoreStart::parse("auto"), None);
        assert_eq!(HardcoreStart::parse("fresh-start"), None);
    }
}
