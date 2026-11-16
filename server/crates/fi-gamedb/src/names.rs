//! Display names of cars ([`crate::Car::name`]): small, explicit corrections over the catalog the
//! extractor produced from the game's database (the game data's `fh2/catalog.json` stays as extracted). The game's
//! `Data_Car` holds a make abbreviation for some makes (`POR`, `CAT`, ...), spells McLaren
//! "Mc Laren", and the extractor split each model from the media name at every capital and digit
//! ("F 1 GT", "MP 412 C"). Every override here is checked by a test: a model override keeps the
//! same letters and digits as the catalog's model (spaces and punctuation only), except the few
//! listed in [`Names::renamed`] with the reason. The corrections are keyed by the game's car ids,
//! so they are the game data's [`NAMES_FILE`], never compiled in.

use serde::Deserialize;

/// The game data file of the corrections.
pub const NAMES_FILE: &str = "fh2/car-names.json";

/// The display corrections (the game data's [`NAMES_FILE`]), each list in its file's order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Names {
    /// Make abbreviations and spellings of `Data_Car`, and their display. An empty display means
    /// no make (`PG`, the game's placeholder make of its traffic bus and trucks).
    pub makes: Vec<(String, String)>,
    /// A car whose make id is filed under another make than its media name's: `(car id, make)`.
    pub car_makes: Vec<(i64, String)>,
    /// Model display by car id.
    pub models: Vec<(i64, String)>,
    /// Model overrides whose letters differ from the catalog's model (case aside), and why.
    pub renamed: Vec<(i64, String)>,
    #[serde(default)]
    source: Option<String>,
}

impl Names {
    /// The corrections of a names JSON.
    pub fn from_json(text: &str) -> Result<Names, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// FH2's corrections ([`NAMES_FILE`]), read once; dereferencing panics with the game data's
/// error, which a process that checked it at start never sees.
pub static NAMES: fi_gamedata::Lazy<Names> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(NAMES_FILE, Names::from_json));

/// The display make of a car with catalog make `make`.
pub fn make_of(car_id: i64, make: &str) -> &str {
    if let Some((_, m)) = NAMES.car_makes.iter().find(|(id, _)| *id == car_id) {
        return m;
    }
    NAMES
        .makes
        .iter()
        .find(|(raw, _)| raw == make)
        .map_or(make, |(_, shown)| shown)
}

/// The display model of a car with catalog model `model`.
pub fn model_of(car_id: i64, model: &str) -> &str {
    NAMES
        .models
        .iter()
        .find(|(id, _)| *id == car_id)
        .map_or(model, |(_, m)| m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Catalog;

    fn letters(s: &str) -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect()
    }

    #[test]
    fn every_override_names_a_catalog_car_and_keeps_its_letters() {
        let c = Catalog::fh2();
        let mut seen = std::collections::BTreeSet::new();
        let n = &*NAMES;
        for (id, model) in &n.models {
            assert!(seen.insert(*id), "car {id} twice");
            let car = c
                .car(*id)
                .unwrap_or_else(|| panic!("car {id} not in the catalog"));
            let renamed = n.renamed.iter().any(|(r, _)| r == id);
            assert_eq!(
                letters(model) == letters(&car.model),
                !renamed,
                "car {id}: {:?} -> {model:?}",
                car.model
            );
            // The media name carries the same model (the extractor's source), renames aside.
            let media = letters(&car.media_name);
            assert!(
                renamed || media.contains(&letters(model)),
                "car {id}: {}",
                car.media_name
            );
        }
        for (id, _) in &n.renamed {
            assert!(
                n.models.iter().any(|(m, _)| m == id),
                "rename {id} without a model"
            );
        }
        for (raw, _) in &n.makes {
            assert!(
                c.cars().any(|car| car.make == *raw),
                "make {raw} not in the catalog"
            );
        }
        for (id, _) in &n.car_makes {
            assert!(c.car(*id).is_some(), "car {id}");
        }
        // No catalog make is left abbreviated (an upper-case make of three letters that is not
        // a real brand written so).
        for car in c.cars() {
            let make = make_of(car.id, &car.make);
            assert!(
                !(make.len() == 3 && make.chars().all(|ch| ch.is_ascii_uppercase()))
                    || ["AMC", "BMW", "KTM", "RAM", "RUF", "SRT", "TVR"].contains(&make),
                "car {}: make {make}",
                car.id
            );
        }
    }

    #[test]
    fn names_read_as_the_game_shows_them() {
        let c = Catalog::fh2();
        let name = |id: i64| c.car(id).unwrap().name();
        assert_eq!(name(1542), "Porsche Cayenne 2012");
        assert_eq!(name(1130), "McLaren MP4-12C 2011");
        assert_eq!(name(338), "McLaren F1 GT 1997");
        assert_eq!(name(2105), "Donkervoort D8 GTO 2013");
        assert_eq!(name(1652), "Caterham R500 2013");
        assert_eq!(name(1651), "Ariel Atom 2013");
        assert_eq!(name(1494), "Bowler Nemesis EXR 2012");
        assert_eq!(name(1532), "Hennessey Venom GT 2012");
        assert_eq!(name(2146), "Local Motors Rally Fighter 2014");
        assert_eq!(name(2101), "Tesla Model S 2014");
        assert_eq!(name(2045), "Ultima GTR 720 2012");
        assert_eq!(name(2393), "Terradyne Gurkha FF 2014");
        assert_eq!(name(2143), "Robby Gordon #7 SST 2013");
        assert_eq!(name(483), "Dodge Viper GTS ACR 1999");
        // The placeholder make: the model alone.
        assert_eq!(name(414), "Bus 2004");
        assert_eq!(name(1674), "Box Truck 2013");
        assert_eq!(name(1215), "Car 2000");
        // Untouched names stay as they were.
        assert_eq!(name(247), "Toyota 2000 GT 1969");
        assert_eq!(name(306), "Shelby Cobra 427 1965");
    }
}
