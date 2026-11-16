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

#[cfg(test)]
mod tests {
    use super::*;
    use fi_playerdb::{FreeCar, GarageCar, PurchasedPart};

    const CATALOG: &str = r#"{"source":{"sha256":"00"},
        "cars":[
          {"id":10,"year":2000,"makeId":1,"make":"M","model":"Starter","mediaName":"M_S","classId":0,"pi":400,"topSpeedMps":60.0,"baseCost":20000},
          {"id":20,"year":2000,"makeId":1,"make":"M","model":"Bought","mediaName":"M_B","classId":0,"pi":500,"topSpeedMps":70.0,"baseCost":120000},
          {"id":30,"year":2000,"makeId":1,"make":"M","model":"Prize","mediaName":"M_P","classId":0,"pi":600,"topSpeedMps":80.0,"baseCost":160000}],
        "classes":[],"buckets":[],"tracks":[],
        "perks":[{"id":0,"cost":1},{"id":3,"cost":2},{"id":12,"cost":5}],
        "starterCars":[10,1218]}"#;
    const PARTS: &str = r#"{"partPrices":{"0":[[20001,5000],[20002,0]],"43":[[7,3700]]}}"#;
    /// The seed held the fallback Z4 (garage id 1), here with flags 0: a seed's cars are free
    /// whatever their flags (the real seed's reads 0x0A, awarded).
    fn seed() -> Seed {
        Seed {
            credits: 20_000,
            winnings: 0,
            level: 0,
            garage: vec![(1, Some(1218))],
        }
    }

    fn car(id: i64, car_id: i64, flags: i64) -> GarageCar {
        GarageCar {
            id,
            car_id: Some(car_id),
            flags: Some(flags),
            parts_value: Some(0),
        }
    }

    fn part(garage: i64, slot: i64, id: i64, paid: i64) -> PurchasedPart {
        PurchasedPart {
            garage_id: Some(garage),
            part_enum: Some(slot),
            part_id: Some(id),
            price_paid: Some(paid),
        }
    }

    /// A legitimate career: the seed's car, the free starter, one bought car, a tombola prize
    /// (0x01), two parts, two perks; 120,000 + 5,000 spent out of 20,000 + 300,000 won with
    /// 190,000 left.
    fn honest() -> PlayerDb {
        PlayerDb {
            garage: vec![
                car(1, 1218, 0),
                car(5, 10, 0x08),
                car(2, 20, 0),
                car(3, 30, 0x09),
            ],
            perks: vec![0, 3],
            purchased_parts: vec![part(2, 0, 20001, 5_000), part(2, 0, 20002, 0)],
            ..PlayerDb::default()
        }
    }

    fn numbers() -> Numbers {
        Numbers {
            credits: Some(190_000),
            winnings: Some(300_000),
            tokens: Some(3),
            level: Some(5),
        }
    }

    fn run(db: &PlayerDb, numbers: Numbers, rules: &GameRules) -> Audit {
        let catalog = Catalog::from_json(CATALOG).unwrap();
        let parts = PartPrices::from_json(PARTS).unwrap();
        audit(
            Ok(db),
            numbers,
            Some(&seed()),
            Reference {
                catalog: &catalog,
                parts: Some(&parts),
                rules,
            },
        )
    }

    #[test]
    fn an_honest_career_scores_nothing() {
        let a = run(&honest(), numbers(), &GameRules::UNKNOWN);
        assert_eq!(a.reasons, []);
        assert_eq!(a.score, 0);
        assert_eq!(
            a.facts,
            Facts {
                unreadable: None,
                cars: Some(4),
                bought: Some(3),
                unpriced: Some(0),
                bought_value: Some(120_000),
                parts: Some(2),
                parts_paid: Some(5_000),
                perks: Some(2),
                perks_cost: Some(3),
                tokens: Some(3),
                budget: Some(20_000 + 300_000 + 250_000),
                vouchers: Some(0),
            }
        );
    }

    #[test]
    fn perks_beyond_the_tokens_or_unknown_block() {
        let mut db = honest();
        db.perks.push(12);
        let a = run(&db, numbers(), &GameRules::UNKNOWN);
        assert_eq!(a.reasons, [ReasonCode::PerksBeyondTokens]);
        assert!(a.would_block());
        db.perks = vec![99];
        assert_eq!(
            run(&db, numbers(), &GameRules::UNKNOWN).reasons,
            [ReasonCode::PerkUnknown]
        );
        // No token count on the revision: only the ids are judged.
        let blind = Numbers {
            tokens: None,
            ..numbers()
        };
        db.perks = vec![12, 3];
        assert_eq!(run(&db, blind, &GameRules::UNKNOWN).reasons, []);
    }

    #[test]
    fn parts_above_list_or_unknown_score_on_base_cars_only() {
        let mut db = honest();
        db.purchased_parts.push(part(2, 0, 20002, 1));
        assert_eq!(
            run(&db, numbers(), &GameRules::UNKNOWN).reasons,
            [ReasonCode::PartAboveList]
        );
        let mut db = honest();
        db.purchased_parts.push(part(2, 5, 1, 0));
        assert_eq!(
            run(&db, numbers(), &GameRules::UNKNOWN).reasons,
            [ReasonCode::PartUnknown]
        );
        // A car-pack car (not in the catalog): its parts are never judged as parts, but what they
        // cost was still spent.
        let mut db = honest();
        db.garage.push(car(4, 9_999, 0x02));
        db.purchased_parts.push(part(4, 5, 1, 999_999_999));
        let a = run(&db, numbers(), &GameRules::UNKNOWN);
        assert_eq!(a.reasons, [ReasonCode::CarsBeyondSpent]);
    }

    #[test]
    fn bought_cars_must_fit_what_the_save_could_spend() {
        // Nothing won: a 160,000 car bought as well makes 285,000 spent out of 270,000 of income.
        let poor = Numbers {
            winnings: Some(0),
            ..numbers()
        };
        let mut db = honest();
        db.garage.push(car(6, 30, 0));
        let a = run(&db, poor, &GameRules::UNKNOWN);
        assert_eq!(a.reasons, [ReasonCode::CarsBeyondSpent]);
        assert_eq!((a.score, a.facts.budget), (20, Some(270_000)));
        assert!(!a.would_block());
        // Five level-up spins of 100,000 explain it.
        let spins = GameRules {
            spin_credits: 100_000,
            ..GameRules::UNKNOWN
        };
        assert_eq!(run(&db, poor, &spins).reasons, []);
        // Review finding 1: credits held from a car sale (outside TotalWinnings) are never read as
        // spending; a seller who bought nothing scores nothing.
        let seller = Numbers {
            credits: Some(9_000_000),
            winnings: Some(0),
            ..numbers()
        };
        assert_eq!(run(&honest(), seller, &GameRules::UNKNOWN).reasons, []);
        // The seed's car and one starter pick are free; a second starter car is not.
        let mut db = honest();
        db.garage.push(car(4, 10, 0));
        assert_eq!(
            db.garage
                .iter()
                .filter(|c| c.origin() == Origin::Bought)
                .count(),
            4
        );
        let a = run(&db, numbers(), &GameRules::UNKNOWN);
        assert_eq!(a.facts.bought_value, Some(140_000));
    }

    #[test]
    fn without_starter_cars_the_bought_rule_waits() {
        let catalog = Catalog::from_json(
            &CATALOG.replace(r#""starterCars":[10,1218]"#, r#""starterCars":null"#),
        )
        .unwrap();
        let rich = Numbers {
            credits: Some(9_000_000),
            ..numbers()
        };
        let a = audit(
            Ok(&honest()),
            rich,
            Some(&seed()),
            Reference {
                catalog: &catalog,
                parts: None,
                rules: &GameRules::UNKNOWN,
            },
        );
        assert_eq!(a.reasons, []);
        assert_eq!((a.facts.bought, a.facts.budget), (Some(3), None));
    }

    #[test]
    fn vouchers_and_unreadable_databases() {
        let mut db = honest();
        db.free_cars.push(FreeCar {
            car_id: 20,
            free_count: Some(0),
        });
        assert_eq!(
            run(&db, numbers(), &GameRules::UNKNOWN).reasons,
            [ReasonCode::VoucherUngranted]
        );
        let catalog = Catalog::from_json(CATALOG).unwrap();
        let a = audit(
            Err(Unreadable::Budget),
            numbers(),
            Some(&seed()),
            Reference {
                catalog: &catalog,
                parts: None,
                rules: &GameRules::UNKNOWN,
            },
        );
        assert_eq!(a.reasons, [ReasonCode::PlayerDbUnreadable]);
        assert_eq!(
            (a.score, a.facts.unreadable.as_deref()),
            (20, Some("PAGE_BUDGET"))
        );
        let missing = Audit::unreadable("BLOB_MISSING", false);
        assert_eq!((missing.score, missing.reasons.len()), (0, 0));
    }

    #[test]
    fn the_embedded_catalog_audits_a_fresh_hardcore_career() {
        // CR-S24 (RE): the starter picks of the opening.
        assert_eq!(Catalog::fh2().starter_cars(), Some(&[289, 461, 1218][..]));
        // The seed's Z4 (flags 0 here; free whatever its flags) and the Supra picked in the opening (flags
        // 0, free): nothing bought, nothing spent, nothing scores.
        let db = PlayerDb {
            garage: vec![car(1, 1218, 0), car(2, 461, 0x08)],
            ..PlayerDb::default()
        };
        let seed = Seed {
            credits: 0,
            winnings: 0,
            level: 0,
            garage: vec![(1, Some(1218))],
        };
        let fresh = Numbers {
            credits: Some(0),
            winnings: Some(0),
            tokens: Some(0),
            level: Some(0),
        };
        let reference = Reference {
            catalog: Catalog::fh2(),
            parts: None,
            rules: &crate::RULES,
        };
        let a = audit(Ok(&db), fresh, Some(&seed), reference);
        assert_eq!(a.reasons, []);
        assert_eq!((a.facts.bought, a.facts.bought_value), (Some(2), Some(0)));
        // A Camaro (289) as well: the second starter was bought (45,000), within the margin.
        let mut more = db.clone();
        more.garage.push(car(3, 289, 0));
        let a = audit(Ok(&more), fresh, Some(&seed), reference);
        assert_eq!((a.reasons.len(), a.facts.bought_value), (0, Some(45_000)));
        // A 6,500,000 Ferrari 250 GTO (249) bought with nothing won is observed.
        more.garage.push(car(4, 249, 0));
        let a = audit(Ok(&more), fresh, Some(&seed), reference);
        assert_eq!(a.reasons, [ReasonCode::CarsBeyondSpent]);
    }

    #[test]
    fn estimates_alone_never_block() {
        // Review finding 2: a voucher, an unknown part and too much bought sum to 80.
        let mut db = honest();
        db.free_cars.push(FreeCar {
            car_id: 20,
            free_count: Some(1),
        });
        db.purchased_parts.push(part(2, 5, 1, 0));
        db.garage.push(car(6, 30, 0));
        let poor = Numbers {
            winnings: Some(0),
            ..numbers()
        };
        let a = run(&db, poor, &GameRules::UNKNOWN);
        assert_eq!(
            a.reasons,
            [
                ReasonCode::PartUnknown,
                ReasonCode::CarsBeyondSpent,
                ReasonCode::VoucherUngranted
            ]
        );
        assert_eq!(a.score, fi_guard::BLOCK - 1);
        assert!(!a.would_block());
        // A deterministic reason lifts the cap.
        db.perks.push(12);
        let a = run(&db, poor, &GameRules::UNKNOWN);
        assert_eq!(a.score, 100);
        assert!(a.would_block());
    }
}
