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
