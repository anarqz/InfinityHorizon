//! Hardcore guardrails (OD-62, ADR 0020; CLOUD_SAVES.md §7.3, GAME_PLATFORM.md §3a "RPC gates"):
//! one pure assessment of a Hardcore profile's progress between two accepted states, shared by the
//! game plane's profile-sync gates (`fi_gamedb::gates`, wall time between syncs) and the
//! save-commit gates (`fi_saves::gates`, play time between revisions). A save and a sync are
//! untrusted input, measured against what the server last accepted.
//!
//! What it decides, given the previous accepted numbers, the new ones, the hours between them and
//! the rates carried at the previous state ([`Input`]):
//! - **ceilings** ([`CeilingCode`]): absurd values, far above any legitimate play, that refuse
//!   from day one whatever the enforcement switch says (an absurd gain at once stays refused;
//!   growth spread over many hours is bounded by the slow ceilings, never refused as a whole);
//! - **the anomaly score** (0-100, [`ReasonCode`]s): rates and conservation gaps that are
//!   recorded with their reason codes and never refuse on their own (shadow) unless the caller's
//!   enforcement switch is on, when a score of [`BLOCK`] or more refuses ([`Assessment::refusal`]).
//!
//! **Rates** are per hour, exponentially weighted moving averages ([`rate`], [`rate_over`]), exact
//! for any cadence: a steady rate `r` reads `r` whether the game syncs every minute or every hour.
//! Two of them per metric:
//! - the **fast** rate, a one-hour time constant, where a gain at once adds itself in full. So
//!   many small syncs cannot hide a burst (the Beta's per-sync allowance let 40 syncs within a
//!   second add 40 x its base), and a burst does not read as an hourly rate of `gain / seconds` (a
//!   burst of saved level-up spins five minutes after the last sync reads at about its own size
//!   per hour, not twelve times it). One measurement's fast step counts at most [`FAST_MAX_HOURS`] (review G2-1: hours
//!   that were waited, not seen played, would let one upload carry `C·h/(1 − e^−h)`);
//! - the **slow** ("day") rate, a [`SLOW_HOURS`] time constant over the measurement's whole hours,
//!   with its own, much lower ceilings ([`CeilingCode::DayUnexplained`] and the rest): what bounds
//!   play sustained just below the fast ceilings (an hourly sync just under them), which the fast
//!   rate alone lets through for ever.
//!
//! The game plane stores the rates with its baseline; the save line replays them from its history
//! ([`replay`], [`Rates::max`]).
//!
//! The tuned numbers ([`Limits`]) are each server's own **calibration**, data read at run time
//! from [`LIMITS_FILE`], changeable after shadow data (P95/P99 of real Hardcore play);
//! [`RULES_VERSION`] names the rule set an assessment was made with.

use fi_gamedata::Lazy;
use serde::Deserialize;
use std::fmt;

/// The rule set an assessment was made with (stored with every assessment row). Bump it whenever a
/// threshold, a score or a rule changes, so calibration compares like with like.
/// `guard-4` (2026-10-31, CR-S81): FH2's levels are driver levels derived from the XP on both
/// paths (a save's, a profile sync's, the game sync a save is compared with), never the reported
/// in-band step, so `LEVEL_XP_MISMATCH` no longer fires for FH2 and band changes are no drops.
pub const RULES_VERSION: &str = "guard-4";

/// The most hours one measurement counts (30 days): a bound on the arithmetic, as every rate has
/// decayed to nothing long before.
pub const MAX_HOURS: f64 = 24.0 * 30.0;

/// The most hours one measurement's **fast** step counts, on both channels (review G2-1). The
/// game syncs during play and the launcher checkpoints at most every five minutes while the save
/// changes, so a longer gap is time the server did not see played: counted in full it would let
/// one upload after `h` hours carry the fast ceiling times `h / (1 − e^−h)` (a maxed career after
/// about 26 hours of lease). The slow rate and the per-race cap see the whole hours.
pub const FAST_MAX_HOURS: f64 = 1.0;

/// The slow rate's time constant, hours: what play sustained over a day reads as.
pub const SLOW_HOURS: f64 = 24.0;

// --- Score bands (the study's suggestion; calibration) ---

/// From this score an assessment is worth observing (20-39).
pub const OBSERVE: u8 = 20;
/// From this score a moderator should review it (40-59).
pub const REVIEW: u8 = 40;
/// From this score it would be blocked (60-100): refused only when the enforcement switch is on.
pub const BLOCK: u8 = 60;

// --- The tuned limits (calibration; deploy-time data, never compiled in) ---

/// The data file holding the [`Limits`] (in the data directory `FI_GAME_DATA_DIR` names, listed in
/// its `manifest.json`; crate fi-gamedata).
pub const LIMITS_FILE: &str = "guard/limits.json";

/// The tuned limits of the assessment: rate bands, ceilings, rate flags and allowances. They are
/// data an operator supplies with the game data ([`LIMITS_FILE`]), so a server's calibration stays
/// its own; the public tree ships neutral values. Every number is per hour unless it says
/// otherwise. A missing or malformed file fails at start (`fi_surfaces::gamedata::load`).
#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Credits rates above which, in order, [`ReasonCode::CreditsRate2To5M`],
    /// [`ReasonCode::CreditsRate5To10M`], [`ReasonCode::CreditsRate10To25M`] and
    /// [`ReasonCode::CreditsRate25MPlus`] score (ascending).
    pub credits_rate_bands: [f64; 4],
    /// Credits per hour, whatever explains them ([`CeilingCode::Credits`]; a gain of more than this
    /// at once reads above it).
    pub credits_ceiling: f64,
    /// Unexplained credits per hour: credits gained beyond race winnings and the level-up spin
    /// allowance ([`CeilingCode::CreditsUnexplained`]). Where winnings are unknown (a save's
    /// summary), every credit gained beyond the level-up spin allowance is unexplained.
    pub unexplained_ceiling: f64,
    /// Race winnings per hour ([`CeilingCode::Winnings`]). Winnings are as untrusted as credits;
    /// without this, credits "explained" by inflated winnings would only meet `credits_ceiling`.
    pub winnings_ceiling: f64,
    /// XP per hour ([`CeilingCode::Xp`]); the game's own limit is checked before this.
    pub xp_ceiling: f64,
    /// Levels per hour ([`CeilingCode::Levels`]); the game's own limit is checked before this.
    pub levels_ceiling: f64,
    /// Unexplained credits per hour of the slow rate ([`CeilingCode::DayUnexplained`]): what
    /// bounds play sustained just below the fast ceilings.
    pub day_unexplained_ceiling: f64,
    /// Race winnings per hour of the slow rate ([`CeilingCode::DayWinnings`]).
    pub day_winnings_ceiling: f64,
    /// XP per hour of the slow rate ([`CeilingCode::DayXp`]).
    pub day_xp_ceiling: f64,
    /// Levels per hour of the slow rate ([`CeilingCode::DayLevels`]).
    pub day_levels_ceiling: f64,
    /// XP per hour above which [`ReasonCode::XpRateHigh`].
    pub xp_rate_high: f64,
    /// Levels per hour above which [`ReasonCode::LevelsRateHigh`].
    pub levels_rate_high: f64,
    /// Cars per hour above which [`ReasonCode::CarsRateHigh`]. Never a refusal: the right measure is
    /// cars without an origin, which needs the save's garage sources.
    pub cars_rate_high: f64,
    /// Unexplained credits per hour above which [`ReasonCode::CreditsWithoutWinnings`], so a race
    /// payout whose credits and winnings reach the server in different syncs never scores. A burst
    /// of saved spins reads as unexplained and scores, never a refusal below
    /// `unexplained_ceiling`.
    pub unexplained_margin: f64,
    /// Races one measurement may hold: one at once, plus this many per hour of the measurement's
    /// hours. [`GameRules::race_credits`] and [`GameRules::race_xp`] are per race, so a measurement
    /// may gain at most that many times them ([`ReasonCode::RaceCapExceeded`]); a count of
    /// payouts, not of events.
    pub races_per_hour: f64,
    /// Credits a save may hold above the profile's last accepted sync at once (saved spins written
    /// before the game's next sync; [`ReasonCode::SaveAheadOfGame`]).
    pub ahead_credits: i64,
    /// ...plus this much per hour since that sync.
    pub ahead_credits_per_hour: f64,
    /// XP a save may hold above the last accepted sync at once.
    pub ahead_xp: i64,
    /// ...plus this much per hour since that sync.
    pub ahead_xp_per_hour: f64,
    /// XP a save may hold below the last accepted sync (the game syncs a race before the launcher
    /// seals its save); with a game's rules at least two of its races ([`GameRules::race_xp`];
    /// [`ReasonCode::SaveBehindGameXp`]).
    pub behind_xp: i64,
}

impl Limits {
    /// Parses [`LIMITS_FILE`]: every field present, finite and not negative, the credits bands
    /// ascending.
    pub fn from_json(text: &str) -> Result<Limits, String> {
        let limits: Limits = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let floats = [
            limits.credits_ceiling,
            limits.unexplained_ceiling,
            limits.winnings_ceiling,
            limits.xp_ceiling,
            limits.levels_ceiling,
            limits.day_unexplained_ceiling,
            limits.day_winnings_ceiling,
            limits.day_xp_ceiling,
            limits.day_levels_ceiling,
            limits.xp_rate_high,
            limits.levels_rate_high,
            limits.cars_rate_high,
            limits.unexplained_margin,
            limits.races_per_hour,
            limits.ahead_credits_per_hour,
            limits.ahead_xp_per_hour,
        ];
        let bands = limits.credits_rate_bands;
        if floats
            .iter()
            .chain(&bands)
            .any(|v| !v.is_finite() || *v < 0.0)
        {
            return Err("every limit must be a finite number, 0 or more".into());
        }
        if limits.ahead_credits < 0 || limits.ahead_xp < 0 || limits.behind_xp < 0 {
            return Err("ahead_credits, ahead_xp and behind_xp must be 0 or more".into());
        }
        if bands.windows(2).any(|w| w[0] > w[1]) {
            return Err("credits_rate_bands must be ascending".into());
        }
        Ok(limits)
    }
}

/// The process's [`Limits`], read from [`LIMITS_FILE`] on first use.
pub static LIMITS: Lazy<Limits> = Lazy::new(|| fi_gamedata::parse(LIMITS_FILE, Limits::from_json));

/// Levels a state may lag behind its XP on the curve ([`ReasonCode::LevelXpMismatch`]) at least: a
/// level-up the game has not applied yet when it synced. With a game's rules the lag allowed is
/// the levels one race's XP crosses ([`GameRules::race_xp`]: one 60k race crosses four Yellow
/// levels; review G2-6).
pub const LEVEL_LAG: i64 = 1;

/// A run of level-ups on a game's XP curve: `steps` level-ups, the `k`-th of them (from 0,
/// restarting in each band) costing `xp_per_level + (k + 1) · increase_per_level` XP, as FH2's
/// `PlayerXPLevels.xml` parser builds its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelBand {
    /// Level-ups in the band.
    pub steps: i64,
    /// The band's `XPPerLevel`.
    pub xp_per_level: i64,
    /// The band's `XPIncreasePerLevel` (0 for a flat band).
    pub increase_per_level: i64,
}

/// A game's level curve: the level a total XP reaches, as the game stores it (the level-ups whose
/// cumulative XP is at most the XP). After the last band the level stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelCurve {
    /// The bands, in order from level 0.
    pub bands: &'static [LevelBand],
}

impl LevelCurve {
    /// The level `xp` reaches: how many level-ups, band after band, its total covers.
    pub fn level_of(&self, xp: i64) -> i64 {
        let mut level = 0;
        let mut needed: i64 = 0;
        for band in self.bands {
            for k in 0..band.steps.max(0) {
                let cost = band
                    .xp_per_level
                    .saturating_add(band.increase_per_level.saturating_mul(k + 1));
                needed = needed.saturating_add(cost.max(1));
                if needed > xp {
                    return level;
                }
                level += 1;
            }
        }
        level
    }

    /// The total XP the last level-up needs (where the curve ends).
    pub fn top_xp(&self) -> i64 {
        self.bands.iter().fold(0i64, |sum, b| {
            (0..b.steps.max(0)).fold(sum, |s, k| {
                s.saturating_add(
                    b.xp_per_level
                        .saturating_add(b.increase_per_level.saturating_mul(k + 1))
                        .max(1),
                )
            })
        })
    }
}

/// A game's skill-token schedule: tokens are earned at skill-point thresholds `first`,
/// `first + step`, `first + 2·step`, ..., at most `cap` of them; the points since the last
/// threshold are the unspent bank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenSchedule {
    /// Skill points of the first token.
    pub first: i64,
    /// Skill points of every further token.
    pub step: i64,
    /// The most tokens ever granted (0: no cap).
    pub cap: i64,
}

impl TokenSchedule {
    /// Tokens earned with `points` skill points (thresholds passed), before the cap.
    pub fn tokens(&self, points: i64) -> i64 {
        if points < self.first {
            0
        } else if self.step <= 0 {
            1
        } else {
            1 + (points - self.first) / self.step
        }
    }

    /// Whether `skills` is a state the game produces: the tokens earned are those the points
    /// passed (at most the cap), and below the cap the unspent bank is what is left since the
    /// last threshold (less than a step).
    pub fn fits(&self, skills: &Skills) -> bool {
        let earned = self.tokens(skills.points.max(0));
        let expected = if self.cap > 0 {
            earned.min(self.cap)
        } else {
            earned
        };
        if skills.tokens != expected {
            return false;
        }
        if self.cap > 0 && earned > self.cap {
            return true;
        }
        let since = if skills.tokens == 0 {
            skills.points
        } else {
            skills.points - (self.first + (skills.tokens - 1) * self.step)
        };
        skills.unspent == since && (skills.tokens == 0 || skills.unspent < self.step)
    }
}

/// A save's skill state (FH2: ProfileData `Main/NumSkillTokens`, SaveState `SkillsState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skills {
    /// Tokens earned (not held: spending them on perks does not lower it).
    pub tokens: i64,
    /// Lifetime skill points.
    pub points: i64,
    /// Skill points not yet turned into a token.
    pub unspent: i64,
}

/// What a game's own data says about its economy (game-derived values live with the game's
/// catalog, `fi_gamedb::RULES` for FH2, never in this crate). [`GameRules::UNKNOWN`] checks nothing
/// that needs them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameRules {
    /// Credits one level-up may bring besides race winnings (FH2: one tombola spin per level-up,
    /// at most its largest credit prize). 0: none.
    pub spin_credits: i64,
    /// Most base credits one race pays (0: no per-race check).
    pub race_credits: i64,
    /// Most XP one race pays (0: no per-race check).
    pub race_xp: i64,
    /// The level curve (`None`: no level check).
    pub level_curve: Option<LevelCurve>,
    /// The skill-token schedule (`None`: no token check).
    pub tokens: Option<TokenSchedule>,
}

impl GameRules {
    /// No game data: no spin allowance, no per-race caps, no level curve, no token schedule.
    pub const UNKNOWN: GameRules = GameRules {
        spin_credits: 0,
        race_credits: 0,
        race_xp: 0,
        level_curve: None,
        tokens: None,
    };

    /// Whether `level` fits `xp` on the curve: at most the curve's level, and no more behind it
    /// than one race's XP ([`GameRules::race_xp`]) or [`LEVEL_LAG`] allow; `true` without a curve.
    pub fn level_fits(&self, xp: i64, level: i64) -> bool {
        self.level_deviation(xp, level) == 0
    }

    /// How far `level` is outside what its `xp` allows on the curve (0: it fits, or no curve): the
    /// levels above the curve's, or below the lowest a level-up not applied yet allows.
    pub fn level_deviation(&self, xp: i64, level: i64) -> i64 {
        let Some(c) = self.level_curve else {
            return 0;
        };
        let expected = c.level_of(xp);
        let lowest = c
            .level_of(xp.saturating_sub(self.race_xp.max(0)))
            .min(expected.saturating_sub(LEVEL_LAG));
        if level > expected {
            level.saturating_sub(expected)
        } else if level < lowest {
            lowest.saturating_sub(level)
        } else {
            0
        }
    }

    /// Whether `skills` fits the token schedule; `true` without one.
    pub fn skills_fit(&self, skills: &Skills) -> bool {
        self.skills_deviation(skills) == 0
    }

    /// How far `skills` is from the token schedule (0: it fits, or no schedule): the tokens it has
    /// too many or too few, at least 1 when only the unspent bank is off.
    pub fn skills_deviation(&self, skills: &Skills) -> i64 {
        let Some(t) = self.tokens else {
            return 0;
        };
        if t.fits(skills) {
            return 0;
        }
        let earned = t.tokens(skills.points.max(0));
        let expected = if t.cap > 0 { earned.min(t.cap) } else { earned };
        skills
            .tokens
            .saturating_sub(expected)
            .saturating_abs()
            .max(1)
    }
}

/// Levels a save may hold below the last accepted sync at least; with a game's curve, the levels
/// the XP allowance crosses below the sync's XP (review G2-8).
pub const BEHIND_LEVELS: i64 = 1;

/// Why an assessment scored: a stable code (the contract's, CLOUD_SAVES.md §7.3; stored in
/// `integrity_assessment.reasons`, shown to moderators and to the launcher) and its score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReasonCode {
    /// Credits per hour in the first of [`Limits::credits_rate_bands`].
    CreditsRate2To5M,
    /// Credits per hour in the second band.
    CreditsRate5To10M,
    /// Credits per hour in the third band.
    CreditsRate10To25M,
    /// Credits per hour above the last band (explained by winnings or level-up spins, or it would
    /// have refused: unexplained credits above [`Limits::unexplained_ceiling`] are a ceiling).
    CreditsRate25MPlus,
    /// Credits gained beyond race winnings and the level-up spin allowance (needs winnings on both
    /// sides: the profile sync's `totalWinnings`, a save's `TotalWinnings`).
    CreditsWithoutWinnings,
    /// Winnings or XP gained in one measurement beyond the game's per-race maximum times the
    /// races it can hold ([`Limits::races_per_hour`]).
    RaceCapExceeded,
    /// The level is not the one the game's XP curve gives its XP (the game loads both as stored
    /// and never recomputes one from the other, so an edited level or XP stays inconsistent).
    /// Deterministic: a conservation rule, it reaches [`BLOCK`] on its own. FH2 reports no
    /// driver level to check (CR-S81: a save's `Main/Level` and a sync's
    /// `playerRaceHistory[0].level` are both the in-band step), so its callers derive the level
    /// from the XP and this code is not emitted for FH2 since `guard-4`; it stays for stored rows
    /// and for a game whose reported level is the curve's.
    LevelXpMismatch,
    /// A save's skill tokens are not those its skill points earned (FH2: tokens at 2,000 skill
    /// points, then one per 100,000; the unspent bank the points since the last token). The game
    /// never recounts them on load, so an edited token count stays inconsistent. Deterministic: a
    /// conservation rule, it reaches [`BLOCK`] on its own.
    TokensSkillMismatch,
    /// XP above [`Limits::xp_rate_high`] per hour.
    XpRateHigh,
    /// Levels above [`Limits::levels_rate_high`] per hour.
    LevelsRateHigh,
    /// Cars above [`Limits::cars_rate_high`] per hour.
    CarsRateHigh,
    /// A save's credits or XP far above the game's last accepted sync (an offline save editor).
    SaveAheadOfGame,
    /// A save's XP or level below the game's last accepted sync (an older save brought back: the
    /// study's rollback threat; credits may legitimately go down, XP and level never do).
    SaveBehindGameXp,
    /// XP or level below the previous accepted state (review G2-3: lowering the level and gaining
    /// it back would earn the level-up spin allowance again; an older save brought back). A fact
    /// check: the same drop, or a smaller one, is judged once ([`Judged::drop_floor`]).
    XpLevelDrop,
    /// A number the previous accepted state had is missing from the new one (review G2-4: removing
    /// `TotalWinnings` or the skill state would switch its check off).
    FieldMissing,
    /// The save audit (OD-67, ADR 0021): the perks unlocked in PlayerDatabase cost more skill
    /// tokens than the save earned (Σ `Perks.Cost` over `PerkUnlock` > `NumSkillTokens`; the game
    /// spends a token only on a perk). Deterministic: it reaches [`BLOCK`] on its own.
    PerksBeyondTokens,
    /// The save audit: a `PerkUnlock` id the game's perk table does not have. Deterministic.
    PerkUnknown,
    /// The save audit: a purchased part paid more than its list price (every discount the game
    /// applies, boards, the perk and the server's sales, only lowers it). Deterministic.
    PartAboveList,
    /// The save audit: a purchased part of a base-database car whose upgrade slot or id the game's
    /// part tables do not have (car-pack cars are not judged; a car pack may add part rows the
    /// base tables lack, so an estimate that observes).
    PartUnknown,
    /// The save audit: the bought cars' base prices and the parts paid exceed the save's income
    /// since its seed (credits then, plus winnings and level-up spins since). Car sales, tombola
    /// credits, achievements and gifts also fund purchases, so an estimate that observes.
    CarsBeyondSpent,
    /// The save audit: a `FreeCars` voucher, which only our economy service can grant and never
    /// has.
    VoucherUngranted,
    /// The save audit: PlayerDatabase could not be read (`fi_playerdb::Unreadable`): observed,
    /// never more on its own (ADR 0021).
    PlayerDbUnreadable,
}

/// The score of a set of reasons: their scores summed, at most 100.
pub fn score_of(reasons: &[ReasonCode]) -> u8 {
    reasons
        .iter()
        .map(|r| u32::from(r.score()))
        .sum::<u32>()
        .min(100) as u8
}

impl ReasonCode {
    /// Every code, in the order an assessment lists them.
    pub const ALL: [ReasonCode; 22] = [
        ReasonCode::CreditsRate2To5M,
        ReasonCode::CreditsRate5To10M,
        ReasonCode::CreditsRate10To25M,
        ReasonCode::CreditsRate25MPlus,
        ReasonCode::CreditsWithoutWinnings,
        ReasonCode::RaceCapExceeded,
        ReasonCode::LevelXpMismatch,
        ReasonCode::TokensSkillMismatch,
        ReasonCode::XpRateHigh,
        ReasonCode::LevelsRateHigh,
        ReasonCode::CarsRateHigh,
        ReasonCode::SaveAheadOfGame,
        ReasonCode::SaveBehindGameXp,
        ReasonCode::XpLevelDrop,
        ReasonCode::FieldMissing,
        ReasonCode::PerksBeyondTokens,
        ReasonCode::PerkUnknown,
        ReasonCode::PartAboveList,
        ReasonCode::PartUnknown,
        ReasonCode::CarsBeyondSpent,
        ReasonCode::VoucherUngranted,
        ReasonCode::PlayerDbUnreadable,
    ];

    /// The stable code.
    pub const fn code(self) -> &'static str {
        match self {
            ReasonCode::CreditsRate2To5M => "CREDITS_RATE_2_5M",
            ReasonCode::CreditsRate5To10M => "CREDITS_RATE_5_10M",
            ReasonCode::CreditsRate10To25M => "CREDITS_RATE_10_25M",
            ReasonCode::CreditsRate25MPlus => "CREDITS_RATE_25M_PLUS",
            ReasonCode::CreditsWithoutWinnings => "CREDITS_WITHOUT_WINNINGS",
            ReasonCode::RaceCapExceeded => "RACE_CAP_EXCEEDED",
            ReasonCode::LevelXpMismatch => "LEVEL_XP_MISMATCH",
            ReasonCode::TokensSkillMismatch => "TOKENS_SKILL_MISMATCH",
            ReasonCode::XpRateHigh => "XP_RATE_HIGH",
            ReasonCode::LevelsRateHigh => "LEVELS_RATE_HIGH",
            ReasonCode::CarsRateHigh => "CARS_RATE_HIGH",
            ReasonCode::SaveAheadOfGame => "SAVE_AHEAD_OF_GAME",
            ReasonCode::SaveBehindGameXp => "SAVE_BEHIND_GAME_XP",
            ReasonCode::XpLevelDrop => "XP_LEVEL_DROP",
            ReasonCode::FieldMissing => "FIELD_MISSING",
            ReasonCode::PerksBeyondTokens => "PERKS_BEYOND_TOKENS",
            ReasonCode::PerkUnknown => "PERK_UNKNOWN",
            ReasonCode::PartAboveList => "PART_ABOVE_LIST",
            ReasonCode::PartUnknown => "PART_UNKNOWN",
            ReasonCode::CarsBeyondSpent => "CARS_BEYOND_SPENT",
            ReasonCode::VoucherUngranted => "VOUCHER_UNGRANTED",
            ReasonCode::PlayerDbUnreadable => "PLAYERDB_UNREADABLE",
        }
    }

    /// What it adds to the score (credits without origin +40, the credits bands +5, +10, +40 and
    /// +60, the third band being a review on its own; a level that does not match its XP is
    /// deterministic, so it would block on its own).
    pub const fn score(self) -> u8 {
        match self {
            ReasonCode::CreditsRate2To5M => 5,
            ReasonCode::CreditsRate5To10M => 10,
            ReasonCode::CreditsRate10To25M => 40,
            ReasonCode::CreditsRate25MPlus => 60,
            ReasonCode::CreditsWithoutWinnings => 40,
            ReasonCode::RaceCapExceeded => 40,
            ReasonCode::LevelXpMismatch => 60,
            ReasonCode::TokensSkillMismatch => 60,
            ReasonCode::XpRateHigh => 10,
            ReasonCode::LevelsRateHigh => 10,
            ReasonCode::CarsRateHigh => 10,
            ReasonCode::SaveAheadOfGame => 40,
            ReasonCode::SaveBehindGameXp => 30,
            ReasonCode::XpLevelDrop => 60,
            ReasonCode::FieldMissing => 60,
            ReasonCode::PerksBeyondTokens => 60,
            ReasonCode::PerkUnknown => 60,
            ReasonCode::PartAboveList => 60,
            ReasonCode::PartUnknown => 20,
            ReasonCode::CarsBeyondSpent => 20,
            ReasonCode::VoucherUngranted => 40,
            ReasonCode::PlayerDbUnreadable => 20,
        }
    }

    /// The code named `text`.
    pub fn parse(text: &str) -> Option<ReasonCode> {
        Self::ALL.into_iter().find(|c| c.code() == text)
    }
}

impl fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// An absurd value that refuses whatever the enforcement switch says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CeilingCode {
    /// Credits above [`Limits::credits_ceiling`] per hour.
    Credits,
    /// Unexplained credits above [`Limits::unexplained_ceiling`] per hour.
    CreditsUnexplained,
    /// Race winnings above [`Limits::winnings_ceiling`] per hour.
    Winnings,
    /// XP above [`Limits::xp_ceiling`] per hour.
    Xp,
    /// Levels above [`Limits::levels_ceiling`] per hour.
    Levels,
    /// Unexplained credits above [`Limits::day_unexplained_ceiling`] per hour of the slow rate.
    DayUnexplained,
    /// Race winnings above [`Limits::day_winnings_ceiling`] per hour of the slow rate.
    DayWinnings,
    /// XP above [`Limits::day_xp_ceiling`] per hour of the slow rate.
    DayXp,
    /// Levels above [`Limits::day_levels_ceiling`] per hour of the slow rate.
    DayLevels,
}

impl CeilingCode {
    /// Every ceiling, in the order they are checked (the first one hit is the refusal).
    pub const ALL: [CeilingCode; 9] = [
        CeilingCode::Credits,
        CeilingCode::CreditsUnexplained,
        CeilingCode::Winnings,
        CeilingCode::Xp,
        CeilingCode::Levels,
        CeilingCode::DayUnexplained,
        CeilingCode::DayWinnings,
        CeilingCode::DayXp,
        CeilingCode::DayLevels,
    ];

    /// The stable code (`SAVES.GATE_REFUSED.rule`, `integrity_assessment.refusal`).
    pub const fn code(self) -> &'static str {
        match self {
            CeilingCode::Credits => "CREDITS_CEILING",
            CeilingCode::CreditsUnexplained => "CREDITS_UNEXPLAINED_CEILING",
            CeilingCode::Winnings => "WINNINGS_CEILING",
            CeilingCode::Xp => "XP_CEILING",
            CeilingCode::Levels => "LEVEL_CEILING",
            CeilingCode::DayUnexplained => "CREDITS_UNEXPLAINED_DAY_CEILING",
            CeilingCode::DayWinnings => "WINNINGS_DAY_CEILING",
            CeilingCode::DayXp => "XP_DAY_CEILING",
            CeilingCode::DayLevels => "LEVEL_DAY_CEILING",
        }
    }

    /// The rule id of a refused sync's fact (`progression.recorded` `refusal`, EVENTS.md §3.3): the
    /// code in lower case.
    pub const fn rule(self) -> &'static str {
        match self {
            CeilingCode::Credits => "credits_ceiling",
            CeilingCode::CreditsUnexplained => "credits_unexplained_ceiling",
            CeilingCode::Winnings => "winnings_ceiling",
            CeilingCode::Xp => "xp_ceiling",
            CeilingCode::Levels => "level_ceiling",
            CeilingCode::DayUnexplained => "credits_unexplained_day_ceiling",
            CeilingCode::DayWinnings => "winnings_day_ceiling",
            CeilingCode::DayXp => "xp_day_ceiling",
            CeilingCode::DayLevels => "level_day_ceiling",
        }
    }

    /// Its limit, per hour ([`LIMITS`]).
    pub fn limit(self) -> f64 {
        match self {
            CeilingCode::Credits => LIMITS.credits_ceiling,
            CeilingCode::CreditsUnexplained => LIMITS.unexplained_ceiling,
            CeilingCode::Winnings => LIMITS.winnings_ceiling,
            CeilingCode::Xp => LIMITS.xp_ceiling,
            CeilingCode::Levels => LIMITS.levels_ceiling,
            CeilingCode::DayUnexplained => LIMITS.day_unexplained_ceiling,
            CeilingCode::DayWinnings => LIMITS.day_winnings_ceiling,
            CeilingCode::DayXp => LIMITS.day_xp_ceiling,
            CeilingCode::DayLevels => LIMITS.day_levels_ceiling,
        }
    }

    /// What it measures, in English.
    const fn what(self) -> &'static str {
        match self {
            CeilingCode::Credits => "credits",
            CeilingCode::CreditsUnexplained => "credits without winnings or level-up spins",
            CeilingCode::Winnings => "race winnings",
            CeilingCode::Xp => "XP",
            CeilingCode::Levels => "levels",
            CeilingCode::DayUnexplained => {
                "credits without winnings or level-up spins, sustained over a day,"
            }
            CeilingCode::DayWinnings => "race winnings, sustained over a day,",
            CeilingCode::DayXp => "XP, sustained over a day,",
            CeilingCode::DayLevels => "levels, sustained over a day,",
        }
    }
}

/// The `rule` of a refusal by score (`SAVES.GATE_REFUSED.rule`, `integrity_assessment.refusal`).
pub const ANOMALY_CODE: &str = "ANOMALY_SCORE";
/// The rule id of a sync refused by score (`progression.recorded` `refusal`).
pub const ANOMALY_RULE: &str = "anomaly_score";

/// A ceiling an assessment hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ceiling {
    /// Which.
    pub code: CeilingCode,
    /// The measured rate, per hour.
    pub rate: f64,
}

/// Why an assessment refuses.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal {
    /// An absurd value (always).
    Ceiling(Ceiling),
    /// A score of [`BLOCK`] or more with the enforcement switch on.
    Anomaly {
        /// The score.
        score: u8,
        /// Its reasons.
        reasons: Vec<ReasonCode>,
    },
}

impl Refusal {
    /// The stable code: a [`CeilingCode::code`] or [`ANOMALY_CODE`].
    pub fn code(&self) -> &'static str {
        match self {
            Refusal::Ceiling(c) => c.code.code(),
            Refusal::Anomaly { .. } => ANOMALY_CODE,
        }
    }

    /// The rule id of a refused sync's fact: a [`CeilingCode::rule`] or [`ANOMALY_RULE`].
    pub fn rule(&self) -> &'static str {
        match self {
            Refusal::Ceiling(c) => c.code.rule(),
            Refusal::Anomaly { .. } => ANOMALY_RULE,
        }
    }

    /// An English reason with the numbers and no player data (a strike's, a parked bundle's).
    pub fn reason(&self) -> String {
        match self {
            Refusal::Ceiling(c) => format!(
                "{} at {:.0} per hour, over the ceiling of {:.0}",
                c.code.what(),
                c.rate,
                c.code.limit()
            ),
            Refusal::Anomaly { score, reasons } => {
                let codes: Vec<&str> = reasons.iter().map(|r| r.code()).collect();
                format!("anomaly score {score} ({})", codes.join(", "))
            }
        }
    }
}

/// The numbers of one state. A metric is measured only when both states know it: a save's summary
/// may lack winnings, a profile sync has no garage, a garage upload has nothing but cars.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Numbers {
    /// Credits.
    pub credits: Option<i64>,
    /// XP.
    pub xp: Option<i64>,
    /// Level.
    pub level: Option<i64>,
    /// Lifetime race winnings (the profile sync's `totalWinnings`, a save's `TotalWinnings`).
    pub winnings: Option<i64>,
    /// Cars in the garage.
    pub cars: Option<i64>,
    /// The skill state (a save only, when readable): checked on the state itself.
    pub skills: Option<Skills>,
}

/// The rates (per hour) at a state: what the next measurement carries. Zero is "no recent
/// growth" (a seed, a reset, a row from before the rates). The fast ones ([`rate`], one-hour time
/// constant) and the slow `day_*` ones ([`rate_over`] with [`SLOW_HOURS`]; review G2-1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rates {
    /// Credits.
    pub credits: f64,
    /// Unexplained credits (beyond winnings and the level-up spin allowance).
    pub unexplained: f64,
    /// Race winnings.
    pub winnings: f64,
    /// XP.
    pub xp: f64,
    /// Levels.
    pub levels: f64,
    /// Cars.
    pub cars: f64,
    /// Unexplained credits, slow.
    pub day_unexplained: f64,
    /// Race winnings, slow.
    pub day_winnings: f64,
    /// XP, slow.
    pub day_xp: f64,
    /// Levels, slow.
    pub day_levels: f64,
}

fn sane(v: f64) -> f64 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

impl Rates {
    /// Every rate finite and not negative (a stored value out of range reads as 0).
    pub fn sanitized(self) -> Rates {
        Rates {
            credits: sane(self.credits),
            unexplained: sane(self.unexplained),
            winnings: sane(self.winnings),
            xp: sane(self.xp),
            levels: sane(self.levels),
            cars: sane(self.cars),
            day_unexplained: sane(self.day_unexplained),
            day_winnings: sane(self.day_winnings),
            day_xp: sane(self.day_xp),
            day_levels: sane(self.day_levels),
        }
    }

    /// The higher of the two, metric by metric.
    pub fn max(self, other: Rates) -> Rates {
        let (a, b) = (self.sanitized(), other.sanitized());
        Rates {
            credits: a.credits.max(b.credits),
            unexplained: a.unexplained.max(b.unexplained),
            winnings: a.winnings.max(b.winnings),
            xp: a.xp.max(b.xp),
            levels: a.levels.max(b.levels),
            cars: a.cars.max(b.cars),
            day_unexplained: a.day_unexplained.max(b.day_unexplained),
            day_winnings: a.day_winnings.max(b.day_winnings),
            day_xp: a.day_xp.max(b.day_xp),
            day_levels: a.day_levels.max(b.day_levels),
        }
    }

    /// The rates after one measurement from `previous` to `next`, `hours` apart, with the highest
    /// level accepted at `previous` (`level_high`, [`Input::level_high`]): each measured metric
    /// advanced by its gain, every other kept.
    pub fn advance(
        self,
        previous: &Numbers,
        next: &Numbers,
        hours: f64,
        rules: &GameRules,
        level_high: Option<i64>,
    ) -> Rates {
        self.advanced(&Deltas::between(previous, next, rules, level_high), hours)
    }

    /// [`Rates::advance`] over a step that spans many measurements (a save line's window step,
    /// whose many small steps are not replayed one by one): the fast rates count its whole hours,
    /// not [`FAST_MAX_HOURS`], as its hours are many measurements' play.
    pub fn advance_window(
        self,
        previous: &Numbers,
        next: &Numbers,
        hours: f64,
        rules: &GameRules,
        level_high: Option<i64>,
    ) -> Rates {
        self.advanced_over(
            &Deltas::between(previous, next, rules, level_high),
            hours,
            hours,
        )
    }

    fn advanced(self, d: &Deltas, hours: f64) -> Rates {
        self.advanced_over(d, sane(hours).min(FAST_MAX_HOURS), hours)
    }

    fn advanced_over(self, d: &Deltas, fast_hours: f64, hours: f64) -> Rates {
        let r = self.sanitized();
        let fast = |carried: f64, gain: Option<i64>| {
            gain.map_or(carried, |g| rate(carried, fast_hours, g.max(0) as f64))
        };
        let slow = |carried: f64, gain: Option<i64>| {
            gain.map_or(carried, |g| {
                rate_over(carried, hours, g.max(0) as f64, SLOW_HOURS)
            })
        };
        Rates {
            credits: fast(r.credits, d.credits),
            unexplained: fast(r.unexplained, d.unexplained),
            winnings: fast(r.winnings, d.winnings),
            xp: fast(r.xp, d.xp),
            levels: fast(r.levels, d.levels),
            cars: fast(r.cars, d.cars),
            day_unexplained: slow(r.day_unexplained, d.unexplained),
            day_winnings: slow(r.day_winnings, d.winnings),
            day_xp: slow(r.day_xp, d.xp),
            day_levels: slow(r.day_levels, d.levels),
        }
    }
}

/// A rate per hour carried `hours` later with `gain` earned evenly over those hours: an
/// exponentially weighted moving average with a one-hour time constant,
/// `previous · e^(−h) + gain · (1 − e^(−h)) / h` (`gain` in full at `h = 0`): [`rate_over`] with a
/// time constant of one hour.
pub fn rate(previous: f64, hours: f64, gain: f64) -> f64 {
    rate_over(previous, hours, gain, 1.0)
}

/// A rate per hour with a time constant of `tau` hours, carried `hours` later with `gain` earned
/// evenly over them: `previous · e^(−h/τ) + gain · (1 − e^(−h/τ)) / h` (`gain / τ` at `h = 0`).
/// Exact for a piecewise-constant rate whatever the steps: a steady `r` gives `r` at any cadence,
/// and splitting an interval into syncs changes nothing. Negative or non-finite inputs read as 0,
/// hours are capped at [`MAX_HOURS`], a time constant below a second reads as one second.
pub fn rate_over(previous: f64, hours: f64, gain: f64, tau: f64) -> f64 {
    let h = sane(hours).min(MAX_HOURS);
    let tau = if tau.is_finite() {
        tau.max(1.0 / 3_600.0)
    } else {
        1.0
    };
    let decay = (-h / tau).exp();
    // (1 - e^(-h/τ)) / h, written with exp_m1 so it stays exact for small h; its limit at 0 is 1/τ.
    let spread = if h > 0.0 {
        -(-h / tau).exp_m1() / h
    } else {
        1.0 / tau
    };
    sane(previous) * decay + sane(gain) * spread
}

/// The rates at the last of `states` (oldest first; each with the hours since the one before, the
/// first's ignored), starting from zero at the first, with `level_high` the highest level accepted
/// at the first (the spin allowance's high-water mark, carried along): how a save line, which
/// stores no rate, carries one into its next upload.
pub fn replay(
    states: impl IntoIterator<Item = (Numbers, f64)>,
    rules: &GameRules,
    level_high: Option<i64>,
) -> Rates {
    let mut out = Rates::default();
    let mut previous: Option<Numbers> = None;
    let mut high = level_high;
    for (numbers, hours) in states {
        if let Some(p) = previous {
            out = out.advance(&p, &numbers, hours, rules, high);
        }
        high = high_water(high, &[numbers.level]);
        previous = Some(numbers);
    }
    out
}

/// `high` raised to every known level of `levels`.
fn high_water(high: Option<i64>, levels: &[Option<i64>]) -> Option<i64> {
    levels
        .iter()
        .flatten()
        .fold(high, |h, l| Some(h.map_or(*l, |h| h.max(*l))))
}

/// What changed between two states (`next - previous`), each `None` when either side lacks it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Deltas {
    /// Credits.
    pub credits: Option<i64>,
    /// XP.
    pub xp: Option<i64>,
    /// Levels.
    pub levels: Option<i64>,
    /// Race winnings.
    pub winnings: Option<i64>,
    /// Cars.
    pub cars: Option<i64>,
    /// Credits gained beyond winnings gained and [`GameRules::spin_credits`] per level gained above
    /// the highest level accepted before (0 or more; winnings count as 0 where unknown). `None`
    /// when credits are not measured.
    pub unexplained: Option<i64>,
}

fn delta(previous: Option<i64>, next: Option<i64>) -> Option<i64> {
    Some(next?.saturating_sub(previous?))
}

impl Deltas {
    /// `next - previous`, with the game's spin allowance for the unexplained credits: one spin per
    /// level gained above `level_high` (the highest level accepted at `previous`; `None`: the
    /// previous level), so a level lowered and gained back earns no spin again (review G2-3).
    pub fn between(
        previous: &Numbers,
        next: &Numbers,
        rules: &GameRules,
        level_high: Option<i64>,
    ) -> Deltas {
        let credits = delta(previous.credits, next.credits);
        let winnings = delta(previous.winnings, next.winnings);
        let levels = delta(previous.level, next.level);
        let spun = high_water(level_high, &[previous.level]);
        let spins = delta(spun, next.level)
            .unwrap_or(0)
            .max(0)
            .saturating_mul(rules.spin_credits.max(0));
        let unexplained = credits.map(|c| {
            c.max(0)
                .saturating_sub(winnings.unwrap_or(0).max(0))
                .saturating_sub(spins)
                .max(0)
        });
        Deltas {
            credits,
            xp: delta(previous.xp, next.xp),
            levels,
            winnings,
            cars: delta(previous.cars, next.cars),
            unexplained,
        }
    }
}

/// The profile's last accepted game sync, which a save is compared with (the cross-channel check).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameSync {
    /// Its credits.
    pub credits: i64,
    /// Its XP.
    pub xp: i64,
    /// Its level: the driver level on the game's curve (FH2 derives it from the XP, CR-S81;
    /// `fi_saves::gates::sync_with_driver_level`).
    pub level: i64,
    /// Hours from it to the save's upload (wall time on the database clock).
    pub hours_since: f64,
}

/// A save against the game's last accepted sync: the save's numbers minus the sync's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GameGap {
    /// Credits.
    pub credits: i64,
    /// XP.
    pub xp: i64,
    /// Levels.
    pub levels: i64,
    /// Hours since the sync.
    pub hours_since: f64,
}

/// A drop already judged: XP and level of a refused state below the previous accepted one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Floor {
    /// Its XP.
    pub xp: i64,
    /// Its level.
    pub level: i64,
}

/// The fact checks' values already refused since the previous accepted state (CLOUD_SAVES.md
/// §7.1 rule 4, review G2-6: a fact check refuses a value once; the same value, or a smaller one,
/// never scores again, only a larger one is a new fact). The game plane keeps them with the
/// baseline's refusal episode (`integrity_baseline.judged_*`), the save line in its parked
/// bundles; both forget them when a new state is accepted, which carries its own deviations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Judged {
    /// The largest level deviation from the XP curve refused ([`GameRules::level_deviation`]).
    pub level_deviation: i64,
    /// The largest skill-token deviation refused ([`GameRules::skills_deviation`]).
    pub token_deviation: i64,
    /// The lowest drop refused: a drop to this XP and level, or a smaller one, is not scored again.
    pub drop_floor: Option<Floor>,
}

impl Judged {
    /// Both memories together: the larger deviations, the lower floor.
    pub fn merge(self, other: Judged) -> Judged {
        Judged {
            level_deviation: self.level_deviation.max(other.level_deviation),
            token_deviation: self.token_deviation.max(other.token_deviation),
            drop_floor: match (self.drop_floor, other.drop_floor) {
                (Some(a), Some(b)) => Some(Floor {
                    xp: a.xp.min(b.xp),
                    level: a.level.min(b.level),
                }),
                (a, b) => a.or(b),
            },
        }
    }
}

/// What [`assess`] measures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Input {
    /// The previous accepted state.
    pub previous: Numbers,
    /// The new one.
    pub next: Numbers,
    /// Hours between the two: wall time between syncs (the game plane), play time between
    /// revisions (the save line). The fast rates count at most [`FAST_MAX_HOURS`] of them.
    pub hours: f64,
    /// The rates carried at `previous`.
    pub rates: Rates,
    /// The highest level accepted up to `previous` since the line's or the baseline's start (a
    /// fresh career, a reset, a restore; `None`: `previous`'s level): level-up spins are granted
    /// only above it (review G2-3).
    pub level_high: Option<i64>,
    /// The fact checks' values already refused since `previous` ([`Judged`]).
    pub judged: Judged,
    /// For a save: the profile's last accepted game sync, if any.
    pub game: Option<GameSync>,
    /// The game's own economy ([`GameRules::UNKNOWN`] checks nothing that needs it).
    pub rules: GameRules,
}

/// The verdict on one measurement.
#[derive(Clone, Debug, PartialEq)]
pub struct Assessment {
    /// 0-100: the sum of the reasons' scores, at most 100.
    pub score: u8,
    /// Why, in [`ReasonCode::ALL`]'s order.
    pub reasons: Vec<ReasonCode>,
    /// The first ceiling hit, if any (refuses whatever the switch).
    pub ceiling: Option<Ceiling>,
    /// The hours measured (capped at [`MAX_HOURS`]; the fast rates counted at most
    /// [`FAST_MAX_HOURS`] of them).
    pub hours: f64,
    /// What changed.
    pub deltas: Deltas,
    /// The rates after this measurement: what an accepted state carries into the next.
    pub rates: Rates,
    /// The highest level accepted once this state is: what an accepted state carries.
    pub level_high: Option<i64>,
    /// The fact checks' values this assessment judged (its fact reasons' values): what a refusal
    /// by score adds to the memory ([`Assessment::judged_after_refusal`]).
    pub judged: Judged,
    /// For a save with a game sync to compare: the gap.
    pub game: Option<GameGap>,
}

impl Assessment {
    /// A score of [`BLOCK`] or more.
    pub fn would_block(&self) -> bool {
        self.score >= BLOCK
    }

    /// Whether it refuses: a ceiling always; a score of [`BLOCK`] or more only with `enforce`
    /// (the deployment's switch, `FI_GUARD_ENFORCE`; off: shadow, scored and recorded only).
    pub fn refusal(&self, enforce: bool) -> Option<Refusal> {
        if let Some(c) = self.ceiling {
            return Some(Refusal::Ceiling(c));
        }
        (enforce && self.would_block()).then(|| Refusal::Anomaly {
            score: self.score,
            reasons: self.reasons.clone(),
        })
    }

    /// The fact memory after this assessment refused by score (`before`, the memory it was made
    /// with, and the values it judged): the next measurement against the same accepted state does
    /// not score them again. A ceiling's refusal judges no fact.
    pub fn judged_after_refusal(&self, before: Judged) -> Judged {
        before.merge(self.judged)
    }
}

fn credits_band(rate: f64) -> Option<ReasonCode> {
    let [b2, b5, b10, b25] = LIMITS.credits_rate_bands;
    if rate > b25 {
        Some(ReasonCode::CreditsRate25MPlus)
    } else if rate > b10 {
        Some(ReasonCode::CreditsRate10To25M)
    } else if rate > b5 {
        Some(ReasonCode::CreditsRate5To10M)
    } else if rate > b2 {
        Some(ReasonCode::CreditsRate2To5M)
    } else {
        None
    }
}

fn grew(gain: Option<i64>) -> bool {
    gain.is_some_and(|g| g > 0)
}

/// Assesses one measurement. A rate scores, and a ceiling is checked, only when this measurement
/// grew its metric: a sync that only spends never reads as a rate, and a lower state never
/// refuses. Credits going down are never a reason (spending), nor are cars (a car sold); XP or a
/// level going down is ([`ReasonCode::XpLevelDrop`]), and for a save, XP or level below the game's
/// last sync ([`ReasonCode::SaveBehindGameXp`]). The fact checks ([`ReasonCode::LevelXpMismatch`],
/// [`ReasonCode::TokensSkillMismatch`], [`ReasonCode::XpLevelDrop`]) score a value once
/// ([`Judged`]).
pub fn assess(input: &Input) -> Assessment {
    let hours = sane(input.hours).min(MAX_HOURS);
    assess_over(input, hours.min(FAST_MAX_HOURS))
}

/// [`assess`] of a whole career measured in one step (CR-S52, a Hardcore line seeded from another
/// profile's save: CLOUD_SAVES.md §7.6): the fast rates count every hour of `input.hours`, as a
/// window step does ([`Rates::advance_window`]), instead of at most [`FAST_MAX_HOURS`] (an upload's
/// burst). The ceilings, the fact checks and the score are [`assess`]'s.
pub fn assess_career(input: &Input) -> Assessment {
    let hours = sane(input.hours).min(MAX_HOURS);
    assess_over(input, hours)
}

fn assess_over(input: &Input, fast_hours: f64) -> Assessment {
    let rules = &input.rules;
    let (previous, next) = (&input.previous, &input.next);
    let hours = sane(input.hours).min(MAX_HOURS);
    let deltas = Deltas::between(previous, next, rules, input.level_high);
    let rates = input.rates.advanced_over(&deltas, fast_hours, hours);
    let mut reasons = Vec::new();
    let mut judged = Judged::default();
    if grew(deltas.credits)
        && let Some(band) = credits_band(rates.credits)
    {
        reasons.push(band);
    }
    if deltas.winnings.is_some()
        && grew(deltas.unexplained)
        && rates.unexplained > LIMITS.unexplained_margin
    {
        reasons.push(ReasonCode::CreditsWithoutWinnings);
    }
    // One race at once, plus what the hours can hold; each at most the game's per-race payout.
    let races = 1.0 + LIMITS.races_per_hour * hours;
    let over = |gain: Option<i64>, per_race: i64| {
        per_race > 0 && gain.is_some_and(|g| g as f64 > per_race as f64 * races)
    };
    if over(deltas.winnings, rules.race_credits) || over(deltas.xp, rules.race_xp) {
        reasons.push(ReasonCode::RaceCapExceeded);
    }
    // Fact checks on the state: a value scores once, when it is new (larger than the previous
    // accepted state's own deviation and than any refused since).
    let level_dev = |n: &Numbers| match (n.xp, n.level) {
        (Some(xp), Some(level)) => rules.level_deviation(xp, level),
        _ => 0,
    };
    let dev = level_dev(next);
    if dev > level_dev(previous).max(input.judged.level_deviation) {
        reasons.push(ReasonCode::LevelXpMismatch);
        judged.level_deviation = dev;
    }
    let token_dev = |n: &Numbers| n.skills.map_or(0, |s| rules.skills_deviation(&s));
    let dev = token_dev(next);
    if dev > token_dev(previous).max(input.judged.token_deviation) {
        reasons.push(ReasonCode::TokensSkillMismatch);
        judged.token_deviation = dev;
    }
    if grew(deltas.xp) && rates.xp > LIMITS.xp_rate_high {
        reasons.push(ReasonCode::XpRateHigh);
    }
    if grew(deltas.levels) && rates.levels > LIMITS.levels_rate_high {
        reasons.push(ReasonCode::LevelsRateHigh);
    }
    if grew(deltas.cars) && rates.cars > LIMITS.cars_rate_high {
        reasons.push(ReasonCode::CarsRateHigh);
    }
    let game = match (input.game, next.credits, next.xp, next.level) {
        (Some(s), Some(credits), Some(xp), Some(level)) => Some(GameGap {
            credits: credits.saturating_sub(s.credits),
            xp: xp.saturating_sub(s.xp),
            levels: level.saturating_sub(s.level),
            hours_since: sane(s.hours_since).min(MAX_HOURS),
        }),
        _ => None,
    };
    if let (Some(g), Some(s)) = (game, input.game) {
        let credits_room =
            LIMITS.ahead_credits as f64 + LIMITS.ahead_credits_per_hour * g.hours_since;
        let xp_room = LIMITS.ahead_xp as f64 + LIMITS.ahead_xp_per_hour * g.hours_since;
        if g.credits as f64 > credits_room || g.xp as f64 > xp_room {
            reasons.push(ReasonCode::SaveAheadOfGame);
        }
        // Behind: two of the game's races of XP, and the levels that XP crosses on the curve.
        let behind_xp = LIMITS.behind_xp.max(rules.race_xp.max(0).saturating_mul(2));
        let behind_levels = rules.level_curve.map_or(BEHIND_LEVELS, |c| {
            c.level_of(s.xp)
                .saturating_sub(c.level_of(s.xp.saturating_sub(behind_xp)))
                .max(BEHIND_LEVELS)
        });
        if g.xp < -behind_xp || g.levels < -behind_levels {
            reasons.push(ReasonCode::SaveBehindGameXp);
        }
    }
    let dropped = deltas.xp.is_some_and(|d| d < 0) || deltas.levels.is_some_and(|d| d < 0);
    let judged_before = input.judged.drop_floor.is_some_and(|f| {
        next.xp.is_none_or(|xp| xp >= f.xp) && next.level.is_none_or(|l| l >= f.level)
    });
    if dropped && !judged_before {
        reasons.push(ReasonCode::XpLevelDrop);
        judged.drop_floor = Some(Floor {
            xp: next.xp.or(previous.xp).unwrap_or(0),
            level: next.level.or(previous.level).unwrap_or(0),
        });
    }
    let missing = |had: bool, has: bool| had && !has;
    // Cars are left out: no save reader counts them yet while a seed's revision may (the seed
    // import's summary), so every first upload would score; the garage sync has nothing else.
    if missing(previous.winnings.is_some(), next.winnings.is_some())
        || missing(previous.skills.is_some(), next.skills.is_some())
    {
        reasons.push(ReasonCode::FieldMissing);
    }
    let score = score_of(&reasons);
    let ceiling = CeilingCode::ALL.into_iter().find_map(|code| {
        let (gain, measured) = match code {
            CeilingCode::Credits => (deltas.credits, rates.credits),
            CeilingCode::CreditsUnexplained => (deltas.unexplained, rates.unexplained),
            CeilingCode::Winnings => (deltas.winnings, rates.winnings),
            CeilingCode::Xp => (deltas.xp, rates.xp),
            CeilingCode::Levels => (deltas.levels, rates.levels),
            CeilingCode::DayUnexplained => (deltas.unexplained, rates.day_unexplained),
            CeilingCode::DayWinnings => (deltas.winnings, rates.day_winnings),
            CeilingCode::DayXp => (deltas.xp, rates.day_xp),
            CeilingCode::DayLevels => (deltas.levels, rates.day_levels),
        };
        (grew(gain) && measured > code.limit()).then_some(Ceiling {
            code,
            rate: measured,
        })
    });
    Assessment {
        score,
        reasons,
        ceiling,
        hours,
        deltas,
        rates,
        level_high: high_water(input.level_high, &[previous.level, next.level]),
        judged,
        game,
    }
}
