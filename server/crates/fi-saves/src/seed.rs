//! The seed check (CR-S52; CLOUD_SAVES.md §7.6, SAVES_API.md §4.10): may a Hardcore line start
//! from another of the account's profiles' save (typically the player's Noob career)? The owner
//! (2026-10-26): "we run the verification in real time from the existent user save. If identified
//! that the current player progression is unusual, we inform the user and reject it. Then start
//! fresh."
//!
//! A plausibility check with no history: the save alone, measured as one step from an empty
//! career over the source profile's play time recorded by this server (the save itself carries no
//! readable play time). So a self-consistent edited save can pass; what catches it later is the
//! moderation action that retires the seeded line and blocks the account (CORE_API.md §5).
//!
//! The check, in order (the first failing step answers alone, as the upload path does):
//! 1. the numbers the gates need are readable (`SUMMARY_UNREADABLE`);
//! 2. the game's limits, every number non-negative (`LIMITS`);
//! 3. the PlayerDatabase reads (`PLAYERDB_UNREADABLE`, fail closed);
//! 4. winnings and the whole skill state are present (`FIELD_MISSING`: the line's anchor carries
//!    the numbers its conservation rules measure from, as `fresh_start`'s strict default);
//! 5. the guardrails against an empty career ([`fi_guard::assess_career`]: every hour of the play
//!    time counts for the fast rates too, so a whole career is a window step, not a one-hour
//!    burst): any ceiling (fast or day), any fact check ([`gates::FACT_CHECKS`]:
//!    `TOKENS_SKILL_MISMATCH`, `XP_LEVEL_DROP`; not `LEVEL_XP_MISMATCH`, since the level is the
//!    one the save's XP reaches, [`gates::with_driver_level`]), or a score of [`fi_guard::BLOCK`] or more refuses, whatever
//!    `FI_GUARD_ENFORCE` says. With no recorded play time the career is measured at zero hours: a
//!    source that never played on this server can seed only a small career (fail closed);
//! 6. **the career pace** ([`career_reasons`], review H2): XP, levels, winnings and unexplained
//!    credits within what a strong player sustains per engaged hour ([`CAREER_XP_PER_HOUR`] and
//!    the others) over the hours plus one; so the career's size sets a minimum of engaged hours
//!    (`CAREER_XP`, `CAREER_LEVELS`, `CAREER_WINNINGS`, `CAREER_CREDITS_UNEXPLAINED`);
//! 7. a `FreeCars` voucher refuses (`VOUCHER_UNGRANTED`: only our economy service grants one, and
//!    it never has).
//!
//! Pure: no I/O. The server reads the PlayerDatabase facts (`fi-playerdb`) and the play time and
//! hands them in.

use crate::fresh::{PlayerDbFacts, PlayerDbUnreadable};
use crate::gates::{self, GatePolicy};
use crate::model::SaveSummary;
use fi_domain::ProfileId;
use fi_guard::{Assessment, BLOCK, GameRules};

/// The `rule` of a refusal of a `seed_from_profile` commit (`SAVES.GATE_REFUSED.rule`).
pub const NOT_PLAUSIBLE_SEED: &str = "NOT_PLAUSIBLE_SEED";

// --- The career pace (review H2): what a strong player sustains over a whole career, per engaged
// hour, from the figures CLOUD_SAVES.md §7 and `fi_guard` are calibrated on. The guardrails' day
// ceilings are about three times a day's sustained top play and were never meant as career
// averages (fi-guard's G2-1 note); a career is held to the single best hour production ever saw,
// sustained, which no real career averages. Each budget is the rate times (engaged hours + 1): one
// hour's allowance at the start, so a new career's first save always fits. ---

/// XP per engaged hour: the most production saw in one hour (the Beta, 2026-10-16: +251k). Level
/// 999 (128.4M XP) then needs at least 511 engaged hours.
pub const CAREER_XP_PER_HOUR: f64 = 251_000.0;
/// Levels per engaged hour: the most production saw in one hour (+8).
pub const CAREER_LEVELS_PER_HOUR: f64 = 8.0;
/// Race winnings per engaged hour: the study's ~1.2M/h of championship-only play (over total drive
/// time it measured ~0.48M/h).
pub const CAREER_WINNINGS_PER_HOUR: f64 = 1_200_000.0;

/// How a `seed_from_profile` commit is checked: its source profile (recorded as the revision's
/// provenance), the play time the career is measured over and the server's PlayerDatabase reader.
#[derive(Clone, Copy, Debug)]
pub struct SeedFromProfileCheck {
    /// The profile whose save it is (one of the account's own, checked by the caller).
    pub source: ProfileId,
    /// The source profile's play time recorded by this server, in hours.
    pub hours: f64,
    /// Reads the bundle's PlayerDatabase.
    pub facts: crate::fresh::FactsReader,
}

/// The seed check's verdict.
#[derive(Clone, Debug, PartialEq)]
pub struct SeedVerdict {
    /// Why it is refused (stable codes, in the order found); empty when accepted.
    pub reasons: Vec<&'static str>,
    /// The guardrails' assessment (step 5), when the save got that far.
    pub assessment: Option<Assessment>,
    /// The numbers the line's revision 1 records: the save's, with the garage count read.
    pub summary: SaveSummary,
}

impl SeedVerdict {
    /// Accepted: nothing refused it.
    pub fn accepted(&self) -> bool {
        self.reasons.is_empty()
    }

    /// The assessment's score (0 before step 5).
    pub fn score(&self) -> u8 {
        self.assessment.as_ref().map_or(0, |a| a.score)
    }

    /// The hours measured (0 before step 5).
    pub fn hours(&self) -> f64 {
        self.assessment.as_ref().map_or(0.0, |a| a.hours)
    }
}

/// The empty career a seed is measured from: every number zero, winnings and the skill state
/// known (so a save lacking them is `FIELD_MISSING`, and conservation measures from zero), no car.
pub const EMPTY_CAREER: SaveSummary = SaveSummary {
    credits: Some(0),
    xp: Some(0),
    level: Some(0),
    cars: Some(0),
    winnings: Some(0),
    tokens: Some(0),
    skill_points: Some(0),
    unspent_skill_points: Some(0),
};

/// The seed check of `summary` (the verified bundle's numbers) with its PlayerDatabase `facts`,
/// over `hours` of the source profile's play, under the game's limits and rules in `policy`.
pub fn check(
    policy: &GatePolicy,
    summary: &SaveSummary,
    facts: Result<&PlayerDbFacts, PlayerDbUnreadable>,
    hours: f64,
) -> SeedVerdict {
    // The level is the driver level the XP reaches, never ProfileData `Main/Level` (the level-ups
    // inside the current colour band: 0 on a genuine 223,773-XP career; 2026-10-31): the verdict's
    // summary is what the dry run shows and what the line's anchor and RPC baseline start from.
    let summary = SaveSummary {
        cars: facts.ok().map(|f| f.cars),
        ..gates::with_driver_level(&policy.rules, *summary)
    };
    let refuse = |reason: &'static str| SeedVerdict {
        reasons: vec![reason],
        assessment: None,
        summary,
    };
    if !summary.is_complete_for_gates() {
        return refuse("SUMMARY_UNREADABLE");
    }
    let negative = [
        summary.tokens,
        summary.skill_points,
        summary.unspent_skill_points,
    ]
    .iter()
    .any(|v| v.is_some_and(|v| v < 0));
    if gates::check_bounds(policy, &summary).is_err() || negative {
        return refuse(gates::LIMITS_RULE);
    }
    let facts = match facts {
        Ok(f) => f,
        Err(PlayerDbUnreadable) => return refuse("PLAYERDB_UNREADABLE"),
    };
    let skills = [
        summary.tokens,
        summary.skill_points,
        summary.unspent_skill_points,
    ];
    if summary.winnings.is_none() || skills.iter().any(Option::is_none) {
        return refuse(fi_guard::ReasonCode::FieldMissing.code());
    }
    let a = assess(&policy.rules, &summary, hours);
    let mut reasons = Vec::new();
    // A `FreeCars` voucher only our economy service can grant, and it never has (the save
    // audit's `VOUCHER_UNGRANTED`; `fresh_start` refuses one too).
    if facts.vouchers > 0 {
        reasons.push(fi_guard::ReasonCode::VoucherUngranted.code());
    }
    if let Some(c) = a.ceiling {
        reasons.push(c.code.code());
    }
    let career = career_reasons(&summary, &a, hours);
    reasons.extend(career.iter().copied());
    let facts_hit = a.reasons.iter().any(|r| gates::FACT_CHECKS.contains(r));
    if facts_hit || a.score >= BLOCK || a.ceiling.is_some() || !career.is_empty() {
        reasons.extend(a.reasons.iter().map(|r| r.code()));
    }
    SeedVerdict {
        reasons,
        assessment: Some(a),
        summary,
    }
}

/// The career pace's refusals of `summary` over `hours` engaged hours (`a`: its assessment from
/// [`EMPTY_CAREER`], whose `unexplained` delta counts the level-up spins).
pub fn career_reasons(summary: &SaveSummary, a: &Assessment, hours: f64) -> Vec<&'static str> {
    let budget = |rate: f64| rate * (hours.max(0.0) + 1.0);
    let over = |value: Option<i64>, rate: f64| value.is_some_and(|v| v as f64 > budget(rate));
    let mut out = Vec::new();
    if over(summary.xp, CAREER_XP_PER_HOUR) {
        out.push("CAREER_XP");
    }
    if over(summary.level, CAREER_LEVELS_PER_HOUR) {
        out.push("CAREER_LEVELS");
    }
    if over(summary.winnings, CAREER_WINNINGS_PER_HOUR) {
        out.push("CAREER_WINNINGS");
    }
    // Credits beyond winnings and level-up spins per engaged hour: the guard's
    // `unexplained_margin`; car sales, tombola credits and gifts fit well within it over a career.
    if over(a.deltas.unexplained, fi_guard::LIMITS.unexplained_margin) {
        out.push("CAREER_CREDITS_UNEXPLAINED");
    }
    out
}

/// The guardrails' assessment of a whole career from [`EMPTY_CAREER`] over `hours`.
pub fn assess(rules: &GameRules, summary: &SaveSummary, hours: f64) -> Assessment {
    fi_guard::assess_career(&fi_guard::Input {
        previous: gates::numbers(rules, &EMPTY_CAREER),
        next: gates::numbers(rules, summary),
        hours,
        rates: fi_guard::Rates::default(),
        level_high: Some(0),
        judged: fi_guard::Judged::default(),
        game: None,
        rules: *rules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_guard::{LevelBand, LevelCurve, TokenSchedule};

    fn career(credits: i64, xp: i64, level: i64, winnings: i64) -> SaveSummary {
        SaveSummary {
            credits: Some(credits),
            xp: Some(xp),
            level: Some(level),
            cars: None,
            winnings: Some(winnings),
            tokens: Some(0),
            skill_points: Some(0),
            unspent_skill_points: Some(0),
        }
    }

    fn none() -> Result<&'static PlayerDbFacts, PlayerDbUnreadable> {
        static F: PlayerDbFacts = PlayerDbFacts {
            cars: 12,
            perks: 0,
            vouchers: 0,
            event_cars: 0,
            parts_paid: 0,
            barn_finds: 0,
        };
        Ok(&F)
    }

    /// A legitimate long career (50M credits explained by winnings, 30M XP, level 150) over 300
    /// hours passes; the same career claimed after one hour is refused by a ceiling, and with no
    /// recorded play time at all too (fail closed).
    #[test]
    fn a_long_career_passes_over_its_hours_and_not_at_once() {
        let p = GatePolicy::default();
        let c = career(50_000_000, 30_000_000, 150, 60_000_000);
        let v = check(&p, &c, none(), 300.0);
        assert!(v.accepted(), "{:?}", v.reasons);
        assert_eq!(v.summary.cars, Some(12), "the garage count is recorded");
        assert!(v.hours() > 299.0);
        for hours in [1.0, 0.0] {
            let v = check(&p, &c, none(), hours);
            assert!(!v.accepted());
            assert!(v.reasons[0].ends_with("CEILING"), "{:?}", v.reasons);
        }
        // An upload's one-hour view of the same career would be refused: the career view is
        // what makes a seed possible at all.
        let burst = gates::assess(
            &p,
            &EMPTY_CAREER,
            0,
            &c,
            300 * 3_600_000,
            gates::Carried::default(),
            None,
        );
        assert!(burst.ceiling.is_some());
    }

    /// Review H2: a maxed career (level 999, 128.4M XP) is refused at 50 and at 300 engaged hours
    /// (the day ceilings as averages let it through at about 46 h); a plausible 40-level career
    /// over 20 engaged hours passes; one with 20 hours' XP claimed in 2 is refused.
    #[test]
    fn the_career_pace_sets_a_minimum_of_engaged_hours() {
        let p = GatePolicy::default();
        let maxed = career(400_000_000, 128_400_000, 999, 900_000_000);
        for hours in [50.0, 300.0] {
            let v = check(&p, &maxed, none(), hours);
            assert!(v.reasons.contains(&"CAREER_XP"), "{hours}: {:?}", v.reasons);
        }
        assert!(
            check(&p, &maxed, none(), 600.0)
                .reasons
                .contains(&"CAREER_WINNINGS")
        );
        let forty = career(3_000_000, 2_000_000, 40, 8_000_000);
        let v = check(&p, &forty, none(), 20.0);
        assert!(v.accepted(), "{:?}", v.reasons);
        let v = check(&p, &forty, none(), 2.0);
        assert_eq!(v.reasons, ["CAREER_XP", "CAREER_LEVELS", "CAREER_WINNINGS"]);
        let unexplained = career(30_000_000, 2_000_000, 40, 0);
        assert!(
            check(&p, &unexplained, none(), 20.0)
                .reasons
                .contains(&"CAREER_CREDITS_UNEXPLAINED")
        );
    }

    #[test]
    fn structural_refusals_come_first_and_alone() {
        let p = GatePolicy::default();
        let unreadable = SaveSummary {
            xp: None,
            ..career(0, 0, 0, 0)
        };
        assert_eq!(
            check(&p, &unreadable, none(), 10.0).reasons,
            ["SUMMARY_UNREADABLE"]
        );
        assert_eq!(
            check(&p, &career(-1, 0, 0, 0), none(), 10.0).reasons,
            ["LIMITS"]
        );
        let negative_points = SaveSummary {
            skill_points: Some(-5),
            ..career(0, 0, 0, 0)
        };
        assert_eq!(
            check(&p, &negative_points, none(), 10.0).reasons,
            ["LIMITS"]
        );
        assert_eq!(
            check(&p, &career(0, 0, 0, 0), Err(PlayerDbUnreadable), 10.0).reasons,
            ["PLAYERDB_UNREADABLE"]
        );
        let no_winnings = SaveSummary {
            winnings: None,
            ..career(0, 0, 0, 0)
        };
        assert_eq!(
            check(&p, &no_winnings, none(), 10.0).reasons,
            ["FIELD_MISSING"]
        );
        let no_tokens = SaveSummary {
            tokens: None,
            ..career(0, 0, 0, 0)
        };
        assert_eq!(
            check(&p, &no_tokens, none(), 10.0).reasons,
            ["FIELD_MISSING"]
        );
        let v = check(&p, &career(0, 0, 0, 0), none(), 0.0);
        assert!(v.accepted() && v.score() == 0);
        let voucher = PlayerDbFacts {
            vouchers: 1,
            ..PlayerDbFacts::default()
        };
        assert_eq!(
            check(&p, &career(0, 0, 0, 0), Ok(&voucher), 10.0).reasons,
            ["VOUCHER_UNGRANTED"]
        );
    }

    /// The fact checks refuse whatever the hours (tokens the skill points did not earn), with the
    /// game's rules. ProfileData `Main/Level` is not one of them: it is not the driver level
    /// (2026-10-31), so the seed takes the level its XP reaches, whatever `Main/Level` says.
    #[test]
    fn the_fact_checks_refuse_a_seed() {
        const CURVE: LevelCurve = LevelCurve {
            bands: &[LevelBand {
                steps: 50,
                xp_per_level: 1_000,
                increase_per_level: 0,
            }],
        };
        let p = GatePolicy {
            rules: GameRules {
                level_curve: Some(CURVE),
                tokens: Some(TokenSchedule {
                    first: 2_000,
                    step: 100_000,
                    cap: 0,
                }),
                ..GameRules::UNKNOWN
            },
            ..GatePolicy::default()
        };
        let consistent = career(10_000, 5_000, 5, 20_000);
        assert!(check(&p, &consistent, none(), 50.0).accepted());
        for main_level in [0, 40] {
            let v = check(&p, &career(10_000, 5_000, main_level, 20_000), none(), 50.0);
            assert!(v.accepted(), "{main_level}: {:?}", v.reasons);
            assert_eq!(v.summary.level, Some(5), "the level 5,000 XP reaches");
        }
        let tokens = SaveSummary {
            tokens: Some(9),
            skill_points: Some(2_500),
            unspent_skill_points: Some(500),
            ..consistent
        };
        let v = check(&p, &tokens, none(), 50.0);
        assert!(
            v.reasons.contains(&"TOKENS_SKILL_MISMATCH"),
            "{:?}",
            v.reasons
        );
    }

    /// The owner's report (SAURON 2026-10-31, numbers only): a genuine Noob career with
    /// `Main/Level` 0 at 223,773 XP (driver level 15 in game), 24,558 credits, 340,758 winnings
    /// and 7 tokens over 3.4 hours was refused `LEVEL_XP_MISMATCH`. On FH2's curve's shape and
    /// token schedule it is accepted, with the driver level (14 from 0) in the verdict's summary.
    #[test]
    fn a_genuine_career_whose_main_level_is_not_its_driver_level_seeds() {
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
                LevelBand {
                    steps: 850,
                    xp_per_level: 20_000,
                    increase_per_level: 300,
                },
            ],
        };
        let p = GatePolicy {
            rules: GameRules {
                spin_credits: 500_000,
                race_credits: 60_000,
                race_xp: 60_000,
                level_curve: Some(FH2_SHAPE),
                tokens: Some(TokenSchedule {
                    first: 2_000,
                    step: 100_000,
                    cap: 65,
                }),
            },
            ..GatePolicy::default()
        };
        let owner = SaveSummary {
            tokens: Some(7),
            skill_points: Some(650_000),
            unspent_skill_points: Some(48_000),
            ..career(24_558, 223_773, 0, 340_758)
        };
        let v = check(&p, &owner, none(), 3.4);
        assert!(v.accepted(), "{:?}", v.reasons);
        assert_eq!(v.summary.level, Some(14));
        // An edited token count on the same career is still refused.
        let edited = SaveSummary {
            tokens: Some(20),
            ..owner
        };
        let v = check(&p, &edited, none(), 3.4);
        assert!(
            v.reasons.contains(&"TOKENS_SKILL_MISMATCH"),
            "{:?}",
            v.reasons
        );
        // And a 30M-XP career over the same 3.4 hours (a `Main/Level` of 0 hides nothing now).
        let big = career(24_558, 30_000_000, 0, 340_758);
        let v = check(
            &p,
            &SaveSummary {
                tokens: Some(7),
                skill_points: Some(650_000),
                unspent_skill_points: Some(48_000),
                ..big
            },
            none(),
            3.4,
        );
        assert!(!v.accepted());
    }
}
