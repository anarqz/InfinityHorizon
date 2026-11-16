//! The save audit (OD-62 conservation step 2, OD-67, ADR 0021; CLOUD_SAVES.md §7.3): what a
//! Hardcore save's PlayerDatabase says about where its garage, perks and parts came from, judged
//! against the game's own numbers. Pure: the `fi-cli saves-audit` job reads the revision, this
//! scores it, the job records it in shadow (`integrity_assessment`, source `save_audit`).
//!
//! The whole state of one revision is judged on its own, never a delta, so the result does not
//! depend on which revisions were audited before:
//! - **perks** ([`ReasonCode::PerksBeyondTokens`], [`ReasonCode::PerkUnknown`]): the token cost of
//!   the unlocked perks is at most the tokens the save earned (`Main/NumSkillTokens`);
//! - **parts** ([`ReasonCode::PartAboveList`], [`ReasonCode::PartUnknown`]): each part bought for
//!   a base-database car exists in its slot's table and was paid at most its list price;
//! - **bought cars** ([`ReasonCode::CarsBeyondSpent`], an estimate): the base prices of the cars
//!   with no origin bit plus every part paid within the save's income since its seed: `seed
//!   credits + (winnings − seed winnings) + spin credits × levels gained`, plus
//!   [`fi_guard::Limits::unexplained_margin`]. Credits held now are not subtracted (review: credits from
//!   car sales, tombola credit prizes, achievements and gifts are outside `TotalWinnings`, so
//!   "income − credits" flagged honest sellers who bought nothing); those same sources can fund a
//!   purchase, so the rule only observes. Free cars with no origin bit (RE, CR-S24): the seed's
//!   own cars, whatever their flags, and one starter pick of the opening (`IEAutoShowCars`: the
//!   catalog's starter cars; granted with flags 0 and no credit call). Only once the catalog names
//!   the starter cars and the line's seed is known. Tombola car prizes carry 0x01, never bought;
//! - **vouchers** ([`ReasonCode::VoucherUngranted`]): any `FreeCars` row (only our economy service
//!   grants them, and it never has);
//! - **unreadable** ([`ReasonCode::PlayerDbUnreadable`]): observed, nothing else judged.
//!
//! **Score:** estimates ([`ReasonCode::CarsBeyondSpent`], [`ReasonCode::PartUnknown`],
//! [`ReasonCode::VoucherUngranted`], [`ReasonCode::PlayerDbUnreadable`]) never add up to
//! [`fi_guard::BLOCK`] on their own: without a deterministic reason ([`DETERMINISTIC`]) the score is
//! capped one below it (review: two estimates summed to 80).
//!
//! Car-pack cars and their parts are not in the base database: never judged, counted as unpriced.
//! A number the revision lacks (`tokens`, `winnings` on revisions stored before they were read)
//! switches its rule off; it is not a reason by itself.

use crate::Catalog;
use crate::parts::PartPrices;
use fi_guard::{GameRules, LIMITS, ReasonCode, score_of};
use fi_playerdb::{Origin, PlayerDb, Unreadable};
use serde::Serialize;
use std::collections::BTreeMap;

/// The revision's ProfileData numbers (its stored summary).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Numbers {
    /// `Main/Credits`.
    pub credits: Option<i64>,
    /// `Main/TotalWinnings`.
    pub winnings: Option<i64>,
    /// `Main/NumSkillTokens` (tokens earned).
    pub tokens: Option<i64>,
    /// The driver level its `Main/XP` reaches on [`crate::rules::LEVEL_CURVE`] (from 0; never
    /// ProfileData `Main/Level`, which is not the driver level: `fi_saves::gates::with_driver_level`).
    pub level: Option<i64>,
}

/// The seed a line started from (OD-60): what "since the seed" is measured from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seed {
    /// Credits.
    pub credits: i64,
    /// `TotalWinnings`.
    pub winnings: i64,
    /// Level.
    pub level: i64,
    /// Its garage, `(Id, CarId)`: free cars whatever their flags.
    pub garage: Vec<(i64, Option<i64>)>,
}

/// The audit's numbers, stored as the assessment's `audit` document (camelCase; CORE_API.md §5
/// `GuardAudit`). `null` where a rule did not run.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    /// `fi_playerdb::Unreadable::code`, or why there was nothing to read (`BLOB_MISSING`, ...).
    pub unreadable: Option<String>,
    /// Garage cars.
    pub cars: Option<i64>,
    /// Cars with no origin bit (the starter pick included).
    pub bought: Option<i64>,
    /// Bought cars not in the base database (car packs): not priced.
    pub unpriced: Option<i64>,
    /// Σ `BaseCost` of the priced bought cars, the seed's cars and the starter pick excepted.
    pub bought_value: Option<i64>,
    /// Purchased parts.
    pub parts: Option<i64>,
    /// Σ `PricePaid` over them.
    pub parts_paid: Option<i64>,
    /// Unlocked perks.
    pub perks: Option<i64>,
    /// Σ `Perks.Cost` over the known ones.
    pub perks_cost: Option<i64>,
    /// Tokens earned (`NumSkillTokens`).
    pub tokens: Option<i64>,
    /// The save's income since its seed (credits then, winnings and level-up spins since, the
    /// margin): what its bought cars and parts may cost at most.
    pub budget: Option<i64>,
    /// `FreeCars` rows.
    pub vouchers: Option<i64>,
}

/// One audit: reasons in [`ReasonCode::ALL`] order, their score and the facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audit {
    /// Why it scored.
    pub reasons: Vec<ReasonCode>,
    /// [`fi_guard::score_of`] the reasons.
    pub score: u8,
    /// The numbers it judged.
    pub facts: Facts,
}

impl Audit {
    /// An audit of a revision whose PlayerDatabase could not be read (or fetched: `code` then
    /// names why, and nothing scores).
    pub fn unreadable(code: &str, scores: bool) -> Audit {
        let reasons = if scores {
            vec![ReasonCode::PlayerDbUnreadable]
        } else {
            Vec::new()
        };
        Audit {
            score: score_of(&reasons),
            reasons,
            facts: Facts {
                unreadable: Some(code.to_owned()),
                ..Facts::default()
            },
        }
    }

    /// Score 60 or more.
    pub fn would_block(&self) -> bool {
        self.score >= fi_guard::BLOCK
    }
}

/// What the audit judges with: the catalog (car prices, perk costs, starter cars), the part
/// prices (`None`: the part rules are off), the game's rules (the spin credits per level).
#[derive(Clone, Copy, Debug)]
pub struct Reference<'a> {
    /// The game's catalog.
    pub catalog: &'a Catalog,
    /// The parts shop's list prices.
    pub parts: Option<&'a PartPrices>,
    /// The game's economy rules.
    pub rules: &'a GameRules,
}

/// Audits one revision's PlayerDatabase (`db`, or why it could not be read).
pub fn audit(
    db: Result<&PlayerDb, Unreadable>,
    numbers: Numbers,
    seed: Option<&Seed>,
    reference: Reference<'_>,
) -> Audit {
    let db = match db {
        Ok(db) => db,
        Err(e) => return Audit::unreadable(e.code(), true),
    };
    let Reference {
        catalog,
        parts,
        rules,
    } = reference;
    let mut reasons = Vec::new();
    let mut facts = Facts {
        cars: Some(db.garage.len() as i64),
        parts: Some(db.purchased_parts.len() as i64),
        perks: Some(db.perks.len() as i64),
        tokens: numbers.tokens,
        vouchers: Some(db.free_cars.len() as i64),
        ..Facts::default()
    };

    // Perks: every id known, their cost within the tokens earned.
    if catalog.has_perks() {
        let mut cost = 0i64;
        for id in &db.perks {
            match catalog.perk_cost(*id) {
                Some(c) => cost = cost.saturating_add(c),
                None => reasons.push(ReasonCode::PerkUnknown),
            }
        }
        facts.perks_cost = Some(cost);
        if numbers.tokens.is_some_and(|t| cost > t) {
            reasons.push(ReasonCode::PerksBeyondTokens);
        }
    }

    // Parts: those of a base-database car exist and cost at most their list price.
    let garage: BTreeMap<i64, Option<i64>> = db.garage.iter().map(|c| (c.id, c.car_id)).collect();
    let paid: i64 = db
        .purchased_parts
        .iter()
        .filter_map(|p| p.price_paid)
        .map(|p| p.max(0))
        .fold(0, i64::saturating_add);
    facts.parts_paid = Some(paid);
    if let Some(prices) = parts {
        for part in &db.purchased_parts {
            let base_car = part
                .garage_id
                .and_then(|g| garage.get(&g).copied().flatten())
                .is_some_and(|car| catalog.stock_car(car).is_some());
            let (Some(slot), Some(id)) = (part.part_enum, part.part_id) else {
                continue;
            };
            if !base_car {
                continue;
            }
            match prices.price(slot, id) {
                None => reasons.push(ReasonCode::PartUnknown),
                Some(list) if part.price_paid.is_some_and(|p| p > list) => {
                    reasons.push(ReasonCode::PartAboveList)
                }
                Some(_) => {}
            }
        }
    }

    // Bought cars, the seed's and the starter pick excepted, within what the save could spend
    // since its seed.
    let mut bought: Vec<_> = db
        .garage
        .iter()
        .filter(|c| c.origin() == Origin::Bought)
        .collect();
    bought.sort_by_key(|c| c.id);
    facts.bought = Some(bought.len() as i64);
    if let (Some(starters), Some(seed)) = (catalog.starter_cars(), seed) {
        bought.retain(|c| !seed.garage.contains(&(c.id, c.car_id)));
        if let Some(at) = bought
            .iter()
            .position(|c| c.car_id.is_some_and(|id| starters.contains(&id)))
        {
            bought.remove(at);
        }
        let (mut value, mut unpriced) = (0i64, 0i64);
        for car in &bought {
            match car
                .car_id
                .and_then(|id| catalog.stock_car(id))
                .and_then(|c| c.base_cost)
            {
                Some(price) => value = value.saturating_add(price),
                None => unpriced += 1,
            }
        }
        facts.bought_value = Some(value);
        facts.unpriced = Some(unpriced);
        if let (Some(winnings), Some(level)) = (numbers.winnings, numbers.level) {
            let spins = rules
                .spin_credits
                .saturating_mul(level.saturating_sub(seed.level).max(0));
            let budget = seed
                .credits
                .saturating_add(winnings.saturating_sub(seed.winnings).max(0))
                .saturating_add(spins)
                .saturating_add(LIMITS.unexplained_margin as i64);
            facts.budget = Some(budget);
            if value.saturating_add(paid) > budget {
                reasons.push(ReasonCode::CarsBeyondSpent);
            }
        }
    }

    if !db.free_cars.is_empty() {
        reasons.push(ReasonCode::VoucherUngranted);
    }

    reasons.sort();
    reasons.dedup();
    Audit {
        score: capped(&reasons),
        reasons,
        facts,
    }
}

/// The reasons that are facts of the game, not estimates: only they may reach
/// [`fi_guard::BLOCK`].
pub const DETERMINISTIC: [ReasonCode; 3] = [
    ReasonCode::PerksBeyondTokens,
    ReasonCode::PerkUnknown,
    ReasonCode::PartAboveList,
];

/// [`score_of`], capped one below [`fi_guard::BLOCK`] without a deterministic reason.
fn capped(reasons: &[ReasonCode]) -> u8 {
    let score = score_of(reasons);
    if reasons.iter().any(|r| DETERMINISTIC.contains(r)) {
        score
    } else {
        score.min(fi_guard::BLOCK - 1)
    }
}
