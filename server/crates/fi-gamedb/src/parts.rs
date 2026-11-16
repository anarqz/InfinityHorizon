//! The parts shop's list prices (CR-S24): `List_Upgrade*.Price` by row id, one table per upgrade
//! slot (`PartEnum` 0-42) and `List_Wheels` (43), as `tools/gamedata/extract_fh2.py --parts`
//! writes the game data's `fh2/parts.json`. A part the game records (`Career_PurchasedParts`: `PartEnum`,
//! `PartId`, `PricePaid`) was paid at most its list price: boards smashed, the "Discount Parts"
//! perk and the server's sales only lower it, and 0 is legal. Every row is kept, price 0
//! included, so an id missing from its table is a part the game does not have.

use serde::Deserialize;
use std::collections::BTreeMap;

/// The game data file of the prices (`FI_GAME_DATA_DIR`), read with the `parts` feature only.
#[cfg(feature = "parts")]
pub const PARTS_FILE: &str = "fh2/parts.json";

#[cfg(feature = "parts")]
static FH2: fi_gamedata::Lazy<PartPrices> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(PARTS_FILE, PartPrices::from_json));

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    part_prices: BTreeMap<String, Vec<(i64, i64)>>,
}

/// List prices by `PartEnum`, then `PartId`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartPrices {
    by_slot: BTreeMap<i64, BTreeMap<i64, i64>>,
}

impl PartPrices {
    /// The FH2 prices of the game data ([`PARTS_FILE`]), parsed once; panics with the game
    /// data's error ([`PartPrices::try_fh2`]).
    #[cfg(feature = "parts")]
    pub fn fh2() -> &'static PartPrices {
        &FH2
    }

    /// [`PartPrices::fh2`], or why the game data has none.
    #[cfg(feature = "parts")]
    pub fn try_fh2() -> Result<&'static PartPrices, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// Prices from the extractor's JSON (tests build small ones). A key that is not a number is
    /// an error.
    pub fn from_json(text: &str) -> Result<PartPrices, String> {
        let raw: Raw = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut by_slot = BTreeMap::new();
        for (slot, rows) in raw.part_prices {
            let slot: i64 = slot.parse().map_err(|_| format!("part slot {slot:?}"))?;
            by_slot.insert(slot, rows.into_iter().collect());
        }
        Ok(PartPrices { by_slot })
    }

    /// The list price of part `id` in slot `part_enum`; `None` when the game has no such part.
    pub fn price(&self, part_enum: i64, id: i64) -> Option<i64> {
        self.by_slot.get(&part_enum)?.get(&id).copied()
    }

    /// Rows in every slot.
    pub fn len(&self) -> usize {
        self.by_slot.values().map(BTreeMap::len).sum()
    }

    /// No rows at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_tables_for_tests() {
        let p = PartPrices::from_json(r#"{"partPrices":{"0":[[1,0],[2,500]],"43":[[7,3700]]}}"#)
            .unwrap();
        assert_eq!(p.price(0, 2), Some(500));
        assert_eq!(p.price(0, 1), Some(0));
        assert_eq!(p.price(0, 3), None);
        assert_eq!(p.price(43, 7), Some(3_700));
        assert_eq!(p.len(), 3);
        assert!(PartPrices::from_json(r#"{"partPrices":{"x":[]}}"#).is_err());
    }

    #[cfg(feature = "parts")]
    #[test]
    fn the_embedded_prices_are_the_games() {
        let p = PartPrices::fh2();
        assert_eq!(p.len(), 37_735);
        assert_eq!(p.by_slot.len(), 44);
        assert_eq!(p.price(0, 247_001), Some(5_000));
    }
}
