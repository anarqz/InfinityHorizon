//! Hardcore progression gates on saves (CLOUD_SAVES.md §7), first ported from ForzaInfinityBeta
//! `server/src/forza/gates.rs` and `server/src/db/reset.rs` (`check_upload`), now the guardrails
//! of OD-62 (ADR 0020, CLOUD_SAVES.md §7.3).
//!
//! A Hardcore upload's numbers are checked in three steps (the hybrid checks, CLOUD_SAVES.md §7.5):
//! 1. **the game's own limits** ([`check_bounds`]): a number negative or beyond them refuses;
//! 2. at the upload, **the inline checks** ([`inline_checks`]): the fact checks and the one-step
//!    ceilings against the revision it is measured against, no history replayed; either refuses
//!    whatever the enforcement switch (owner 2026-10-27);
//! 3. nightly, on the committed revision, **the guardrails** ([`assess`], `fi_guard`): the upload
//!    against that revision over the line's **play time** between the two (the time some session
//!    held a live lease, credited by the store under its lock on the database clock,
//!    [`crate::lease::play_credit`]; an idle or asleep PC earns nothing; the fast rates count at
//!    most [`fi_guard::FAST_MAX_HOURS`] of it, review G2-1), with what the line carries at that
//!    revision ([`Carried`]: the rates and the level high-water mark replayed from the line's
//!    history, the facts its parked refusals judged) and the profile's game sync. Ceilings and,
//!    with [`GatePolicy::enforce`] (`FI_GUARD_ENFORCE`), a score of 60 or more are **flagged**
//!    for moderation; nothing is refused there.
//!
//! The Beta's allowances (credits 540k + 620k/h of play, XP 384k + 503k/h, levels 12 + 16/h,
//! cars 6 + 20/h) refused legitimate bursts and no longer refuse anything. What stays from the
//! port: a refusal never rolls the player back silently (the upload is refused at once with the
//! reason, the bundle is parked for review, the session's earlier revisions stay; failure 3);
//! unreadable numbers fail closed for Hardcore and park the save; a base number that is missing
//! counts as zero, so an unreadable base can never wave an upload through.

use crate::model::SaveSummary;
use fi_guard::{Assessment, CeilingCode, GameRules, GameSync, Judged, Numbers, Rates, ReasonCode};
use std::fmt;

/// The gates' settings. [`GatePolicy::default`] is shadow mode without a game's rules.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GatePolicy {
    /// Most play time credited between two revisions (720 h): a base revision played on for a year
    /// is measured as a month.
    pub max_hours: f64,
    /// The game's own limits.
    pub max_credits: i64,
    /// The game's own limits.
    pub max_xp: i64,
    /// The game's own limits.
    pub max_level: i64,
    /// `FI_GUARD_ENFORCE`: the nightly pass flags a score of [`fi_guard::BLOCK`] or more. Off by
    /// default (shadow: scored and recorded only); ceilings are flagged either way, and the
    /// inline checks ([`inline_checks`]) refuse whatever it says.
    pub enforce: bool,
    /// The game's economy (FH2's is `fi_gamedb::RULES`, set by the deployment; the default has
    /// none, so no level curve, spin allowance or per-race cap applies).
    pub rules: GameRules,
}

impl Default for GatePolicy {
    fn default() -> Self {
        Self {
            max_hours: fi_guard::MAX_HOURS,
            max_credits: 999_999_999,
            max_xp: 200_000_000,
            max_level: 999,
            enforce: false,
            rules: GameRules::UNKNOWN,
        }
    }
}

/// The `rule` of a refusal by the game's limits (`SAVES.GATE_REFUSED.rule`).
pub const LIMITS_RULE: &str = "LIMITS";

/// Why a save's progress was refused.
#[derive(Clone, Debug, PartialEq)]
pub enum GateRefusal {
    /// A number is negative or beyond the game's limits.
    OutOfBounds {
        /// Which number.
        field: &'static str,
        /// Its value.
        value: i64,
    },
    /// The guardrails refused: a ceiling, or a score with the enforcement switch on.
    Guard(fi_guard::Refusal),
}

impl GateRefusal {
    /// The stable rule code (`SAVES.GATE_REFUSED.rule`): [`LIMITS_RULE`], a ceiling's code or
    /// `ANOMALY_SCORE`.
    pub fn code(&self) -> &'static str {
        match self {
            GateRefusal::OutOfBounds { .. } => LIMITS_RULE,
            GateRefusal::Guard(r) => r.code(),
        }
    }
}

impl fmt::Display for GateRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GateRefusal::OutOfBounds { field, value } => {
                write!(f, "{field} {value} is beyond the game's limits")
            }
            GateRefusal::Guard(r) => f.write_str(&r.reason()),
        }
    }
}

/// The game's limits on `next` (an upload): every readable number at least 0 and at most its
/// limit. The caller refuses a Hardcore upload whose own numbers are incomplete before this.
pub fn check_bounds(policy: &GatePolicy, next: &SaveSummary) -> Result<(), GateRefusal> {
    for (field, value, max) in [
        ("credits", next.credits, policy.max_credits),
        ("xp", next.xp, policy.max_xp),
        ("level", next.level, policy.max_level),
        ("winnings", next.winnings, policy.max_credits),
        ("cars", next.cars, i64::MAX),
    ] {
        if let Some(v) = value
            && !(0..=max).contains(&v)
        {
            return Err(GateRefusal::OutOfBounds { field, value: v });
        }
    }
    Ok(())
}

/// `s` with its level derived from its XP on the game's curve (the driver level the save's XP
/// reaches, from 0: the game shows it plus one), when the rules have a curve and both the XP and
/// `Main/Level` read; else `s` unchanged (an unreadable `Main/Level` stays unreadable, so the
/// summary's fail-closed `SUMMARY_UNREADABLE` is exactly what it was).
///
/// ProfileData `Main/Level` is **not** the driver level (SAURON 2026-10-31, the owner's career:
/// `Main/Level` 0 at XP 223,773, which the game shows as driver level 15, and 1 at XP 232,284).
/// It reads as the level-ups done inside the current colour band (Green starts at 210,000 XP), so
/// it equals the driver level only in Yellow, and drops back to 0 at every band boundary. The
/// server therefore never trusts it: every summary the server keeps or measures takes its level
/// from the XP, and a save has no independent level to check against its XP. A profile sync's
/// `playerRaceHistory[0].level` is the same in-band step (CR-S81's second half), so the game plane
/// derives its level from the XP too (`fi_gamedb::gates::driver_level`) and `LEVEL_XP_MISMATCH`
/// fires on neither path.
pub fn with_driver_level(rules: &GameRules, s: SaveSummary) -> SaveSummary {
    match (rules.level_curve, s.xp, s.level) {
        (Some(curve), Some(xp), Some(_)) => SaveSummary {
            level: Some(curve.level_of(xp)),
            ..s
        },
        _ => s,
    }
}

/// `g` with its level derived from its XP on the game's curve, when the rules have one (CR-S81):
/// the game sync a save is compared with (`SAVE_BEHIND_GAME_XP`) comes from the profile's RPC
/// baseline, whose rows written before the fix hold the in-band step the game reports in
/// `playerRaceHistory[0].level` (0 again at every colour band), never the driver level a save's
/// summary carries ([`with_driver_level`]). Both sides are then on one scale.
pub fn sync_with_driver_level(rules: &GameRules, g: GameSync) -> GameSync {
    match rules.level_curve {
        Some(curve) => GameSync {
            level: curve.level_of(g.xp),
            ..g
        },
        None => g,
    }
}

/// A save's numbers as the guardrails measure them (its skill state when all three numbers are
/// readable), its level derived from its XP ([`with_driver_level`]): stored summaries from before
/// that derivation carry `Main/Level` and are measured on the same scale as new ones.
pub fn numbers(rules: &GameRules, s: &SaveSummary) -> Numbers {
    let s = &with_driver_level(rules, *s);
    let skills = match (s.tokens, s.skill_points, s.unspent_skill_points) {
        (Some(tokens), Some(points), Some(unspent)) => Some(fi_guard::Skills {
            tokens,
            points,
            unspent,
        }),
        _ => None,
    };
    Numbers {
        credits: s.credits,
        xp: s.xp,
        level: s.level,
        winnings: s.winnings,
        cars: s.cars,
        skills,
    }
}

/// What a save line carries at the revision an upload is measured against (the service reads it
/// from the line's history and its parked refusals; a save line stores none of it).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Carried {
    /// The rates replayed at the base.
    pub rates: Rates,
    /// The spin allowance's level high-water mark at the base (review G2-3).
    pub level_high: Option<i64>,
    /// The fact checks' values refused by score against the line's head (review G2-6).
    pub judged: Judged,
}

/// Hours of play from `from_ms` to `to_ms` (a counter that went backwards credits zero), at most
/// [`GatePolicy::max_hours`].
pub fn play_hours(policy: &GatePolicy, from_ms: u64, to_ms: u64) -> f64 {
    (to_ms.saturating_sub(from_ms) as f64 / 3_600_000.0).min(policy.max_hours)
}

/// The guardrails' assessment of `next` (an upload within the game's limits) against `base` (the
/// revision it is measured against), given the line's play time at `base` (`base_play_ms`) and
/// now (`play_ms`), what the line carries at `base` and the profile's last accepted game sync.
/// A missing credits, XP or level on the base side counts as zero (fail closed); winnings and cars
/// are measured only when both sides know them, and one the base had but `next` lacks is
/// `FIELD_MISSING` (review G2-4).
pub fn assess(
    policy: &GatePolicy,
    base: &SaveSummary,
    base_play_ms: u64,
    next: &SaveSummary,
    play_ms: u64,
    carried: Carried,
    game: Option<GameSync>,
) -> Assessment {
    let base_numbers = numbers(&policy.rules, base);
    let previous = Numbers {
        credits: Some(base.credits.unwrap_or(0)),
        xp: Some(base.xp.unwrap_or(0)),
        level: Some(base_numbers.level.unwrap_or(0)),
        ..base_numbers
    };
    fi_guard::assess(&fi_guard::Input {
        previous,
        next: numbers(&policy.rules, next),
        hours: play_hours(policy, base_play_ms, play_ms),
        rates: carried.rates,
        level_high: carried.level_high,
        judged: carried.judged,
        game: game.map(|g| sync_with_driver_level(&policy.rules, g)),
        rules: policy.rules,
    })
}

/// The checks the upload path runs under the hybrid Hardcore checks (CLOUD_SAVES.md §7.5), against
/// the gated base alone (no history replayed):
/// - the guardrails' **fact checks** (§7.1 rule 4; owner 2026-10-27 "the fact checks always refuse
///   at upload"): a level its XP does not give, skill tokens the skill points did not earn, XP or
///   a level below the base, against the base and the facts parked refusals already judged;
/// - the **one-step ceilings** (owner 2026-10-27 "one-step ceilings refuse at upload"): the fast
///   ceilings ([`ONE_STEP_CEILINGS`]) on this one step, from zero rates and with the base's own
///   level as the spin mark. Rates never go below zero and grow with what they carry, and the base's
///   level is at most the replayed high-water mark (so at least as many spins are granted): a
///   one-step ceiling is hit only when the full guardrails' replay hits a ceiling too (E1's 900M at
///   once stays refused at once). The day ceilings need the replay and stay nightly.
///
/// The result's reasons are only [`FACT_CHECKS`] (its score theirs), its ceiling a one-step one, its
/// rates and mark cleared: [`Assessment::refusal`] with `enforce = true` is the verdict (a ceiling
/// first, then `ANOMALY_SCORE` for a fact not judged yet), whatever the deployment's switch.
pub fn inline_checks(
    policy: &GatePolicy,
    base: &SaveSummary,
    base_play_ms: u64,
    next: &SaveSummary,
    play_ms: u64,
    judged: Judged,
) -> Assessment {
    let mut a = assess(
        policy,
        base,
        base_play_ms,
        next,
        play_ms,
        Carried {
            judged,
            ..Carried::default()
        },
        None,
    );
    a.reasons.retain(|r| FACT_CHECKS.contains(r));
    a.score = fi_guard::score_of(&a.reasons);
    a.ceiling = a.ceiling.filter(|c| ONE_STEP_CEILINGS.contains(&c.code));
    a.rates = Rates::default();
    a.level_high = None;
    a
}

/// The ceilings the upload path checks on one step ([`inline_checks`]): the fast ones. They come
/// first in [`CeilingCode::ALL`], so the first ceiling a step hits is one of them whenever any is.
pub const ONE_STEP_CEILINGS: [CeilingCode; 5] = [
    CeilingCode::Credits,
    CeilingCode::CreditsUnexplained,
    CeilingCode::Winnings,
    CeilingCode::Xp,
    CeilingCode::Levels,
];

/// The fact checks the upload path runs ([`inline_checks`]); every other reason, and the day
/// ceilings, are the nightly pass's. `LEVEL_XP_MISMATCH` is not one: a save's level is derived
/// from its own XP ([`with_driver_level`]; `Main/Level` is not the driver level), so on a save it
/// would compare the XP with itself. An XP drop is still `XP_LEVEL_DROP`.
pub const FACT_CHECKS: [ReasonCode; 2] = [ReasonCode::TokensSkillMismatch, ReasonCode::XpLevelDrop];

#[cfg(test)]
mod tests {
    use super::*;
    use fi_guard::{CeilingCode, LevelBand, LevelCurve, ReasonCode};

    const HOUR: u64 = 3_600_000;

    fn summary(credits: i64, xp: i64, level: i64) -> SaveSummary {
        SaveSummary {
            credits: Some(credits),
            xp: Some(xp),
            level: Some(level),
            ..SaveSummary::default()
        }
    }

    fn check(base: &SaveSummary, base_ms: u64, next: &SaveSummary, ms: u64) -> Assessment {
        let p = GatePolicy::default();
        check_bounds(&p, next).expect("within the limits");
        assess(&p, base, base_ms, next, ms, Carried::default(), None)
    }

    #[test]
    fn bursts_the_beta_refused_are_accepted() {
        // 1.1M at once (the Beta allowed 540k): no score.
        let a = check(&summary(0, 0, 0), 0, &summary(1_100_000, 0, 0), 0);
        assert_eq!((a.score, a.ceiling), (0, None));
        // 5M at once, 13 levels at once: scored, never refused, enforced or not.
        let a = check(&summary(0, 0, 0), 0, &summary(5_000_000, 0, 13), 0);
        assert_eq!(a.reasons, vec![ReasonCode::CreditsRate2To5M]);
        let enforced = GatePolicy {
            enforce: true,
            ..GatePolicy::default()
        };
        assert!(a.refusal(enforced.enforce).is_none());
        // Spending is fine.
        let a = check(
            &summary(500, 10, 3),
            7 * HOUR,
            &summary(100, 10, 3),
            8 * HOUR,
        );
        assert_eq!((a.score, a.ceiling), (0, None));
    }

    #[test]
    fn absurd_values_are_refused_with_the_reason() {
        // Review E1 as a save: 900M credits over the seed's numbers.
        let a = check(
            &summary(25_000, 1_000, 1),
            0,
            &summary(900_000_000, 1_000, 1),
            HOUR,
        );
        let r = GateRefusal::Guard(a.refusal(false).expect("refused in shadow"));
        assert_eq!(r.code(), "CREDITS_CEILING");
        assert!(r.to_string().starts_with("credits at "), "{r}");
        // XP and levels far beyond any play.
        let a = check(&summary(0, 0, 0), 0, &summary(0, 20_000_000, 0), HOUR);
        assert_eq!(a.ceiling.map(|c| c.code), Some(CeilingCode::Xp));
        let a = check(&summary(0, 0, 0), 0, &summary(0, 0, 500), HOUR);
        assert_eq!(a.ceiling.map(|c| c.code), Some(CeilingCode::Levels));
    }

    #[test]
    fn only_play_time_since_the_base_counts() {
        // The base was committed after 40 h of play; 30 idle days later the line has 40 h still:
        // 30M at once is refused (the unexplained ceiling, 25M per hour of play).
        let a = check(
            &summary(0, 0, 0),
            40 * HOUR,
            &summary(30_000_000, 0, 0),
            40 * HOUR,
        );
        assert_eq!(a.hours, 0.0);
        assert_eq!(
            a.ceiling.map(|c| c.code),
            Some(CeilingCode::CreditsUnexplained)
        );
        // Over ten hours of play it is 3M an hour: a score.
        let a = check(
            &summary(0, 0, 0),
            40 * HOUR,
            &summary(30_000_000, 0, 0),
            50 * HOUR,
        );
        assert_eq!(a.ceiling, None);
        // A play-time counter that went backwards credits zero.
        let a = check(
            &summary(0, 0, 0),
            40 * HOUR,
            &summary(30_000_000, 0, 0),
            HOUR,
        );
        assert_eq!(a.hours, 0.0);
        // Capped.
        let p = GatePolicy::default();
        assert_eq!(play_hours(&p, 0, 365 * 24 * HOUR), 720.0);
    }

    #[test]
    fn a_missing_base_number_counts_as_zero() {
        let unreadable = SaveSummary::default();
        let a = check(&unreadable, 0, &summary(900_000_000, 2_000_000, 40), 0);
        assert!(
            a.ceiling.is_some(),
            "an unreadable base never waves an upload through"
        );
        let a = check(&unreadable, 0, &summary(40_000, 1_000, 2), 0);
        assert_eq!((a.score, a.ceiling), (0, None));
        assert_eq!(SaveSummary::ZERO.credits, Some(0));
    }

    #[test]
    fn limits_and_negatives_are_refused() {
        let p = GatePolicy::default();
        assert_eq!(
            check_bounds(&p, &summary(-1, 0, 0)),
            Err(GateRefusal::OutOfBounds {
                field: "credits",
                value: -1
            })
        );
        let r = check_bounds(&p, &summary(0, 0, 1000)).unwrap_err();
        assert_eq!(
            (r.code(), r.to_string().as_str()),
            ("LIMITS", "level 1000 is beyond the game's limits")
        );
        let negative_winnings = SaveSummary {
            winnings: Some(-5),
            ..summary(0, 0, 0)
        };
        assert!(check_bounds(&p, &negative_winnings).is_err());
    }

    /// The owner's career (SAURON 2026-10-31, numbers only): `Main/Level` 0 at 223,773 XP and 1
    /// at 232,284 XP, shown in game as driver levels 15 and 16. On FH2's curve's shape (Yellow,
    /// 14 x 15,000; then 20,000 a level) the derived levels are 14 and 15 (from 0), every other
    /// number kept; without a curve the summary is left as read.
    #[test]
    fn a_saves_level_is_derived_from_its_xp() {
        const FH2_SHAPE: LevelCurve = LevelCurve {
            bands: &[
                LevelBand {
                    steps: 14,
                    xp_per_level: 15_000,
                    increase_per_level: 0,
                },
                LevelBand {
                    steps: 135,
                    xp_per_level: 20_000,
                    increase_per_level: 0,
                },
            ],
        };
        let rules = GameRules {
            level_curve: Some(FH2_SHAPE),
            ..GameRules::UNKNOWN
        };
        for (xp, main_level, driver) in [(223_773, 0, 14), (232_284, 1, 15), (0, 0, 0)] {
            let read = SaveSummary {
                winnings: Some(340_758),
                tokens: Some(7),
                ..summary(24_558, xp, main_level)
            };
            let derived = with_driver_level(&rules, read);
            assert_eq!(
                derived,
                SaveSummary {
                    level: Some(driver),
                    ..read
                }
            );
            assert_eq!(numbers(&rules, &read).level, Some(driver));
            assert_eq!(with_driver_level(&GameRules::UNKNOWN, read), read);
        }
        // No XP: nothing to derive from (the summary stays unreadable for the gates).
        let no_xp = SaveSummary {
            xp: None,
            ..summary(0, 0, 3)
        };
        assert_eq!(with_driver_level(&rules, no_xp), no_xp);
        // No `Main/Level`: still unreadable (fail closed as before), nothing filled in.
        let no_level = SaveSummary {
            level: None,
            ..summary(0, 223_773, 0)
        };
        assert_eq!(with_driver_level(&rules, no_level), no_level);
        assert!(!with_driver_level(&rules, no_level).is_complete_for_gates());

        // CR-S81's second half: the game sync a save is compared with holds the in-band step
        // the game reported (1 at 232,194 XP, driver 15). Measured on the driver scale: a save at
        // the sync's numbers is level with it (a gap of 0 levels, not +14), and one far behind
        // is still behind.
        let policy = GatePolicy {
            rules,
            ..GatePolicy::default()
        };
        let synced = GameSync {
            credits: 24_558,
            xp: 232_194,
            level: 1,
            hours_since: 0.1,
        };
        assert_eq!(sync_with_driver_level(&rules, synced).level, 15);
        assert_eq!(sync_with_driver_level(&GameRules::UNKNOWN, synced), synced);
        let save = |xp| summary(24_558, xp, 0);
        let a = assess(
            &policy,
            &save(223_773),
            0,
            &save(232_194),
            HOUR,
            Carried::default(),
            Some(synced),
        );
        assert_eq!(a.game.map(|g| (g.xp, g.levels)), Some((0, 0)));
        assert!(
            !a.reasons.contains(&ReasonCode::SaveBehindGameXp),
            "{:?}",
            a.reasons
        );
        let a = assess(
            &policy,
            &save(0),
            0,
            &save(15_000),
            HOUR,
            Carried::default(),
            Some(synced),
        );
        assert_eq!(a.game.map(|g| g.levels), Some(-14));
        assert!(
            a.reasons.contains(&ReasonCode::SaveBehindGameXp),
            "{:?}",
            a.reasons
        );
    }

    #[test]
    fn winnings_and_the_games_rules_are_used_when_known() {
        const CURVE: LevelCurve = LevelCurve {
            bands: &[LevelBand {
                steps: 50,
                xp_per_level: 1_000,
                increase_per_level: 0,
            }],
        };
        let p = GatePolicy {
            rules: GameRules {
                spin_credits: 100_000,
                race_credits: 50_000,
                race_xp: 50_000,
                level_curve: Some(CURVE),
                tokens: Some(fi_guard::TokenSchedule {
                    first: 2_000,
                    step: 100_000,
                    cap: 0,
                }),
            },
            ..GatePolicy::default()
        };
        let with = |credits, xp, level, winnings| SaveSummary {
            winnings: Some(winnings),
            ..summary(credits, xp, level)
        };
        // Credits that follow winnings: nothing; credits without them: conservation scores.
        let a = assess(
            &p,
            &with(0, 5_000, 5, 0),
            0,
            &with(500_000, 5_000, 5, 500_000),
            HOUR,
            Carried::default(),
            None,
        );
        assert!(a.reasons.is_empty(), "{:?}", a.reasons);
        let a = assess(
            &p,
            &with(0, 5_000, 5, 0),
            0,
            &with(500_000, 5_000, 5, 0),
            HOUR,
            Carried::default(),
            None,
        );
        assert_eq!(a.reasons, vec![ReasonCode::CreditsWithoutWinnings]);
        // A save without winnings (stored before they were read) is not compared on them.
        let a = assess(
            &p,
            &summary(0, 5_000, 5),
            0,
            &with(500_000, 5_000, 5, 0),
            HOUR,
            Carried::default(),
            None,
        );
        assert!(a.reasons.is_empty(), "{:?}", a.reasons);
        // ProfileData `Main/Level` is not the driver level (2026-10-31): whatever it says, the
        // level measured is the one the XP reaches, so it never scores on a save.
        for main_level in [0, 30] {
            let a = assess(
                &p,
                &with(0, 5_000, 5, 0),
                0,
                &with(0, 5_000, main_level, 0),
                HOUR,
                Carried::default(),
                None,
            );
            assert!(a.reasons.is_empty(), "{main_level}: {:?}", a.reasons);
            assert_eq!(a.deltas.levels, Some(0), "{main_level}");
        }
        // Skill tokens the skill points did not earn (an edited token count); a consistent state
        // scores nothing.
        let skilled = |tokens, points, unspent| SaveSummary {
            tokens: Some(tokens),
            skill_points: Some(points),
            unspent_skill_points: Some(unspent),
            ..with(0, 5_000, 5, 0)
        };
        let a = assess(
            &p,
            &with(0, 5_000, 5, 0),
            0,
            &skilled(9, 262_450, 60_450),
            HOUR,
            Carried::default(),
            None,
        );
        assert_eq!(a.reasons, vec![ReasonCode::TokensSkillMismatch]);
        let a = assess(
            &p,
            &with(0, 5_000, 5, 0),
            0,
            &skilled(3, 262_450, 60_450),
            HOUR,
            Carried::default(),
            None,
        );
        assert!(a.reasons.is_empty(), "{:?}", a.reasons);
        // Against the game's last sync.
        let synced = GameSync {
            credits: 0,
            xp: 200_000,
            level: 50,
            hours_since: 0.1,
        };
        let a = assess(
            &p,
            &with(0, 50_000, 50, 0),
            0,
            &with(0, 50_000, 50, 0),
            HOUR,
            Carried::default(),
            Some(synced),
        );
        assert!(
            a.reasons.contains(&ReasonCode::SaveBehindGameXp),
            "{:?}",
            a.reasons
        );
    }

    #[test]
    fn a_save_without_a_number_its_base_had_would_block() {
        // Review G2-4: dropping `TotalWinnings` (or the skill state) from an edited ProfileData
        // switched the credits conservation (or the token check) off.
        let with = |winnings, tokens| SaveSummary {
            winnings,
            tokens,
            skill_points: tokens.map(|_| 0),
            unspent_skill_points: tokens.map(|_| 0),
            ..summary(1_000_000, 0, 0)
        };
        let base = with(Some(1_000_000), Some(0));
        for next in [with(None, Some(0)), with(Some(1_000_000), None)] {
            let a = assess(
                &GatePolicy::default(),
                &base,
                0,
                &next,
                HOUR,
                Carried::default(),
                None,
            );
            assert_eq!(a.reasons, vec![ReasonCode::FieldMissing], "{next:?}");
            assert!(a.would_block());
        }
        // A base stored before the field was read: nothing to miss.
        let a = check(&summary(0, 0, 0), 0, &with(None, None), HOUR);
        assert!(a.reasons.is_empty(), "{:?}", a.reasons);
    }

    #[test]
    fn the_inline_checks_need_no_rate_and_no_high_water_mark() {
        // The upload path runs them without the line's replay (CLOUD_SAVES.md §7.5): the fact
        // reasons of the full assessment must not depend on what the replay carries, and a
        // one-step ceiling must be one the replay hits whatever it carries.
        const CURVE: LevelCurve = LevelCurve {
            bands: &[LevelBand {
                steps: 999,
                xp_per_level: 1_000,
                increase_per_level: 0,
            }],
        };
        let p = GatePolicy {
            rules: GameRules {
                level_curve: Some(CURVE),
                tokens: Some(fi_guard::TokenSchedule {
                    first: 2_000,
                    step: 100_000,
                    cap: 0,
                }),
                ..GameRules::UNKNOWN
            },
            ..GatePolicy::default()
        };
        let base = summary(25_000, 30_000, 30);
        let skilled = |credits, xp, level, tokens| SaveSummary {
            tokens: Some(tokens),
            skill_points: Some(262_450),
            unspent_skill_points: Some(60_450),
            ..summary(credits, xp, level)
        };
        let busy = Rates {
            credits: 30_000_000.0,
            unexplained: 30_000_000.0,
            winnings: 9.0,
            xp: 9_000_000.0,
            levels: 200.0,
            cars: 99.0,
            day_unexplained: 9_000_000.0,
            day_winnings: 9.0,
            day_xp: 9_000_000.0,
            day_levels: 99.0,
        };
        for next in [
            summary(25_000, 30_000, 45),
            summary(900_000_000, 1_000, 1),
            skilled(2_000_000, 40_000, 40, 9),
            skilled(25_000, 30_000, 30, 3),
        ] {
            let facts = inline_checks(&p, &base, 0, &next, HOUR, Judged::default());
            for (rates, level_high) in [(Rates::default(), None), (busy, Some(500))] {
                let full = assess(
                    &p,
                    &base,
                    0,
                    &next,
                    HOUR,
                    Carried {
                        rates,
                        level_high,
                        judged: Judged::default(),
                    },
                    None,
                );
                let full_facts: Vec<ReasonCode> = full
                    .reasons
                    .iter()
                    .copied()
                    .filter(|r| FACT_CHECKS.contains(r))
                    .collect();
                assert_eq!(facts.reasons, full_facts, "{next:?}");
                // A one-step ceiling is one the replayed guardrails hit too.
                if facts.ceiling.is_some() {
                    assert!(full.ceiling.is_some(), "{next:?}");
                }
            }
            assert_eq!(facts.score, fi_guard::score_of(&facts.reasons));
        }
        // Each of them alone would block: the upload path refuses on any one.
        assert!(
            inline_checks(
                &p,
                &base,
                0,
                &skilled(25_000, 30_000, 30, 9),
                HOUR,
                Judged::default()
            )
            .refusal(true)
            .is_some()
        );
        // A `Main/Level` its XP does not give is not a fact on a save (its level is derived).
        assert_eq!(
            inline_checks(
                &p,
                &base,
                0,
                &summary(25_000, 30_000, 45),
                HOUR,
                Judged::default()
            )
            .refusal(true),
            None
        );
    }
}
