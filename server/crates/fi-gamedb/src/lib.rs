//! Forza Horizon 2's catalog for the server (SER-M3 step 6, OD-48) and the RPC gates over it.
//!
//! No game data is compiled in (T-1115): every table here is a file of the game data directory
//! (`FI_GAME_DATA_DIR`, crate `fi_gamedata`), read once. [`Catalog::fh2`] is its `fh2/catalog.json`,
//! extracted from the game's `media/stripped/gamedbRC.slt` (and the `Tracks.str` of its English and
//! Brazilian Portuguese stringtables) by `tools/gamedata/extract_fh2.py`. Only what the game plane needs: each car's make, class, display
//! PI and stock top speed (the integrity gates' reference), the classes, the race buckets
//! (drivatars bring a car of the race's bucket, hero cars first) and the tracks (length for the
//! race-time gate, names for server texts). The car packs the server knows (`fh2/packs.json`,
//! the ZephyrSkies FM5/FM6 packs by `tools/gamedata/extract_fh2_packs.py`, OD-231) are merged
//! over it: [`Catalog::car`] finds them (each with its [`Car::pack`]), while [`Catalog::cars`]
//! stays the base game's. Other car-pack ids are not in it: callers treat an unknown id as "no
//! reference", never as a refusal by itself ([`gates`] says where a limit applies).
//!
//! Tests build their own catalogs with [`Catalog::from_json`].
//!
//! [`rules`] holds the game's own economy numbers the Hardcore guardrails measure with (the level
//! curve, the tombola's largest credit prize, the per-race payout limits; OD-62). The catalog also
//! carries each car's `Data_Car.BaseCost`, the perks' token costs and, once the RE names them, the
//! starter-pick cars (CR-S24), which [`audit`] judges a save's PlayerDatabase with; the parts
//! shop's list prices are [`parts`] (`fh2/parts.json`, read only with the `parts`
//! feature: the save audit job's, and the game plane's purchasedParts gate, OD-178).
//!
//! [`topspeed`] is each car's upgraded top-speed bound (`fh2/topspeed.tsv`, numbers only;
//! OD-163), the per-car speed limit of the anti-cheat's peer observations.
//!
//! [`leaderboards`] is the game's board taxonomy (`fh2/leaderboards.json`, T-PD-S3): speed
//! cameras, tracks and Showcases the play dashboard's boards name. [`stats`] is the in-game
//! Statistics screen's layout over the stored online profile (`fh2/stats-layout.json`, T-PD-S2;
//! `fh2/stats-extras.json`, T-PD-S4). [`stats_boards`] is the traced stats boards' mapping and
//! ceilings (`fh2/stats-boards-v2.json`, T-PD-S5), superseding the 4/x rows of
//! [`leaderboards`]; [`card_ids`] the player card's badge and title ids (`fh2/card-ids.json`).
//! [`pi`] is the game's raw performance index, its class bands and its shown PI (T-PD-S6,
//! `fh2/gate-confirmations.json`).

pub mod audit;
pub mod card_ids;
pub mod gates;
pub mod leaderboards;
pub mod names;
pub mod parts;
pub mod pi;
pub mod rules;
pub mod stats;
pub mod stats_boards;
pub mod topspeed;

pub use rules::RULES;

use fi_gamedata::Lazy;
use serde::Deserialize;
use std::collections::BTreeMap;

/// The game data file of the FH2 catalog (`FI_GAME_DATA_DIR`; never in the crate).
pub const CATALOG_FILE: &str = "fh2/catalog.json";

/// The game data file of the car packs' cars (OD-231).
pub const PACKS_FILE: &str = "fh2/packs.json";

static FH2: Lazy<Catalog> = Lazy::new(|| {
    let mut catalog = fi_gamedata::parse(CATALOG_FILE, Catalog::from_json)?;
    fi_gamedata::parse(PACKS_FILE, |text| catalog.add_packs_json(text))?;
    Ok(catalog)
});

/// The car pack a car comes from (absent for a base-game car).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PackOrigin {
    /// The pack's `mod.json` id ("fm5-zs-retro-supercars-pack").
    pub id: String,
    /// The pack's `mod.json` version.
    pub version: String,
    /// The game the pack's cars come from ("FM5", "FM6").
    pub game: String,
}

/// A car of the base database.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Car {
    /// `Data_Car.Id`, the game's car id.
    pub id: i64,
    /// Model year.
    pub year: i64,
    /// `Data_Car.MakeID` (the storefront searches by it).
    pub make_id: i64,
    /// The make's name ("Aston Martin").
    pub make: String,
    /// The model, from the media name ("Supra RZ").
    pub model: String,
    /// `Data_Car.MediaName` ("TOY_SupraRZ_98").
    pub media_name: String,
    /// `Data_Car.ClassID` (stock class).
    pub class_id: i64,
    /// Stock display PI (608 for 0.608).
    pub pi: i64,
    /// Stock top speed, m/s.
    pub top_speed_mps: f64,
    /// `Data_Car.BaseCost`, credits (CR-S24; absent from catalogs extracted before it).
    #[serde(default)]
    pub base_cost: Option<i64>,
    /// The car pack of a pack car ([`PACKS_FILE`]); `None` for a base-game car.
    #[serde(default)]
    pub pack: Option<PackOrigin>,
}

impl Car {
    /// "Toyota Supra RZ 1998": the display make and model ([`names`]: the game's abbreviated
    /// makes spelled out, the extractor's model spacing corrected; `make` and `model` stay as
    /// extracted), then the year.
    pub fn name(&self) -> String {
        let make = names::make_of(self.id, &self.make);
        let model = names::model_of(self.id, &self.model);
        format!("{make} {model} {}", self.year).trim().to_owned()
    }

    /// The PI as the game's score rows carry it (0.608).
    pub fn performance_index(&self) -> f64 {
        self.pi as f64 / 1000.0
    }
}

/// A car class (D, C, B, A, S1, S2, X, R).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarClass {
    /// `CarClasses.Id`.
    pub id: i64,
    /// "S1".
    pub name: String,
    /// The class's highest display PI.
    pub max_pi: i64,
}

/// A track (a circuit, a route, a cross-country course).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    /// `Tracks.id`, the game's track id.
    pub id: i64,
    /// `Tracks.MediaName` (every route's is "Anthem").
    pub media_name: String,
    /// Length, metres.
    pub length_m: i64,
    /// Driven in reverse.
    pub reverse: bool,
    /// The official names by language (`en`, `pt-BR`), when the stringtable has them.
    #[serde(default)]
    pub names: BTreeMap<String, String>,
    /// The lap count of a circuit the game itself races over more than one lap (its online events,
    /// else its career races); `None` for a route.
    #[serde(default)]
    pub circuit_laps: Option<i64>,
    /// Driven with the game's cross-country AI.
    #[serde(default)]
    pub cross_country: bool,
}

impl Track {
    /// The track's name in `lang` (`en` or `pt-BR`), else English, else its media name.
    pub fn name(&self, lang: &str) -> &str {
        self.names
            .get(lang)
            .or_else(|| self.names.get("en"))
            .map_or(self.media_name.as_str(), String::as_str)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BucketRow {
    car_id: i64,
    bucket: i64,
    hero: bool,
}

#[derive(Deserialize)]
struct PerkRow {
    id: i64,
    cost: i64,
}

#[derive(Deserialize)]
struct Source {
    sha256: String,
}

#[derive(Deserialize)]
struct Raw {
    source: Source,
    cars: Vec<Car>,
    classes: Vec<CarClass>,
    buckets: Vec<BucketRow>,
    tracks: Vec<Track>,
    #[serde(default)]
    perks: Vec<PerkRow>,
    #[serde(default, rename = "starterCars")]
    starter_cars: Option<Vec<i64>>,
}

#[derive(Deserialize)]
struct RawPacks {
    cars: Vec<Car>,
}

/// The catalog, indexed.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    sha256: String,
    cars: BTreeMap<i64, Car>,
    pack_cars: BTreeMap<i64, Car>,
    classes: BTreeMap<i64, CarClass>,
    car_bucket: BTreeMap<i64, i64>,
    bucket_cars: BTreeMap<i64, Vec<i64>>,
    tracks: BTreeMap<i64, Track>,
    perks: BTreeMap<i64, i64>,
    starter_cars: Option<Vec<i64>>,
}

impl Catalog {
    /// The FH2 catalog of the game data ([`CATALOG_FILE`] with [`PACKS_FILE`] merged), parsed
    /// once. Panics with the game data's error when it cannot be loaded; a process checks it at
    /// start ([`Catalog::try_fh2`]).
    pub fn fh2() -> &'static Catalog {
        &FH2
    }

    /// [`Catalog::fh2`], or why the game data has none.
    pub fn try_fh2() -> Result<&'static Catalog, &'static fi_gamedata::Error> {
        FH2.load()
    }

    /// Merges car packs' cars (`fh2-packs.json`'s shape) over the catalog: [`Catalog::car`] finds
    /// them, [`Catalog::cars`] does not list them. A pack car never replaces a base-game car.
    pub fn add_packs_json(&mut self, text: &str) -> Result<(), serde_json::Error> {
        let raw: RawPacks = serde_json::from_str(text)?;
        for car in raw.cars {
            if !self.cars.contains_key(&car.id) {
                self.pack_cars.insert(car.id, car);
            }
        }
        Ok(())
    }

    /// A catalog from the extractor's JSON (tests build small ones).
    pub fn from_json(text: &str) -> Result<Catalog, serde_json::Error> {
        let raw: Raw = serde_json::from_str(text)?;
        let mut c = Catalog {
            sha256: raw.source.sha256,
            ..Catalog::default()
        };
        for car in raw.cars {
            c.cars.insert(car.id, car);
        }
        for class in raw.classes {
            c.classes.insert(class.id, class);
        }
        // Hero cars first, then by car id, whatever order the rows came in.
        let mut rows = raw.buckets;
        rows.sort_by_key(|b| (b.bucket, !b.hero, b.car_id));
        for b in rows {
            c.car_bucket.entry(b.car_id).or_insert(b.bucket);
            c.bucket_cars.entry(b.bucket).or_default().push(b.car_id);
        }
        for t in raw.tracks {
            c.tracks.insert(t.id, t);
        }
        for p in raw.perks {
            c.perks.insert(p.id, p.cost);
        }
        c.starter_cars = raw.starter_cars;
        Ok(c)
    }

    /// SHA-256 (hex) of the `gamedbRC.slt` the catalog came from: the game's car database, the
    /// value the hook reports in `X-Fh2-CarDb`.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// A car of the base database or of a known car pack ([`Car::pack`]).
    pub fn car(&self, id: i64) -> Option<&Car> {
        self.cars.get(&id).or_else(|| self.pack_cars.get(&id))
    }

    /// A car of the base database only.
    pub fn stock_car(&self, id: i64) -> Option<&Car> {
        self.cars.get(&id)
    }

    /// Every known car pack's car, by id.
    pub fn pack_cars(&self) -> impl Iterator<Item = &Car> {
        self.pack_cars.values()
    }

    /// Every base-game car, by id.
    pub fn cars(&self) -> impl Iterator<Item = &Car> {
        self.cars.values()
    }

    /// Every car the game can have: the base-game cars by id, then the known car packs' by id.
    pub fn all_cars(&self) -> impl Iterator<Item = &Car> {
        self.cars.values().chain(self.pack_cars.values())
    }

    /// A class.
    pub fn class(&self, id: i64) -> Option<&CarClass> {
        self.classes.get(&id)
    }

    /// Every class, by id.
    pub fn classes(&self) -> impl Iterator<Item = &CarClass> {
        self.classes.values()
    }

    /// A track.
    pub fn track(&self, id: i64) -> Option<&Track> {
        self.tracks.get(&id)
    }

    /// Every track, by id.
    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.values()
    }

    /// The race bucket of a car (`Data_Car_Buckets`), if it races in one.
    pub fn bucket_of(&self, car: i64) -> Option<i64> {
        self.car_bucket.get(&car).copied()
    }

    /// The token cost of perk `id` (`Perks.Cost`), if the game has it.
    pub fn perk_cost(&self, id: i64) -> Option<i64> {
        self.perks.get(&id).copied()
    }

    /// Whether the catalog lists the perks (catalogs extracted before CR-S24 do not).
    pub fn has_perks(&self) -> bool {
        !self.perks.is_empty()
    }

    /// The cars the game's opening offers as the free starter pick (no origin bit in
    /// `Career_Garage.Flags`), `None` until the RE names them (CR-S24).
    pub fn starter_cars(&self) -> Option<&[i64]> {
        self.starter_cars.as_deref()
    }

    /// The cars of a race bucket, hero cars first (empty for an unknown bucket).
    pub fn bucket_cars(&self, bucket: i64) -> &[i64] {
        self.bucket_cars.get(&bucket).map_or(&[], Vec::as_slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_catalog_is_the_games() {
        let c = Catalog::fh2();
        assert_eq!(
            c.sha256(),
            "4d50013dc7705de05a074e4421679e16a3a1eda0816841861b0b7139ecf45517"
        );
        assert_eq!(c.cars().count(), 354);
        let car = c.car(247).expect("Toyota 2000GT");
        assert_eq!(car.name(), "Toyota 2000 GT 1969");
        assert_eq!((car.class_id, car.pi), (0, 330));
        assert!((car.performance_index() - 0.33).abs() < 1e-9);
        assert!(car.top_speed_mps > 60.0);
        assert_eq!(c.class(4).map(|k| k.name.as_str()), Some("S1"));
        let track = c.track(0).expect("Laguna Seca");
        assert_eq!(track.length_m, 3602);
        assert_eq!(track.name("pt-BR"), track.name("en"));
        // Every bucketed car is a known car, and every bucket leads with a hero car.
        for car in c.car_bucket.keys() {
            assert!(c.car(*car).is_some(), "bucketed car {car} is in Data_Car");
        }
        assert_eq!(c.bucket_cars.len(), 28);
        assert!(c.bucket_cars(-1).is_empty());
        assert_eq!(c.car(99_999), None);
        // CR-S24: every car priced, 25 perks costing 65 tokens.
        assert!(c.cars().all(|car| car.base_cost.is_some_and(|p| p > 0)));
        assert_eq!(c.car(297).and_then(|car| car.base_cost), Some(160_000));
        assert_eq!((0..25).filter_map(|id| c.perk_cost(id)).sum::<i64>(), 65);
        assert_eq!(c.perk_cost(25), None);
    }

    #[test]
    fn the_zephyrskies_packs_are_merged() {
        let c = Catalog::fh2();
        assert_eq!(c.pack_cars().count(), 303);
        assert!(c.pack_cars().all(|car| c.stock_car(car.id).is_none()));
        let tr = c.car(255).expect("Ferrari 512 TR");
        assert_eq!(tr.name(), "Ferrari 512 TR 1991");
        let pack = tr.pack.as_ref().expect("a pack car");
        assert_eq!(
            (pack.id.as_str(), pack.game.as_str()),
            ("fm5-zs-retro-supercars-pack", "FM5")
        );
        for id in [9631, 9674] {
            let car = c.car(id).expect("pack car above 4095");
            assert_eq!(car.pack.as_ref().map(|p| p.game.as_str()), Some("FM5"));
            assert!(car.top_speed_mps > 0.0 && c.class(car.class_id).is_some());
        }
        assert!(
            c.pack_cars()
                .all(|car| car.pack.is_some() && c.class(car.class_id).is_some())
        );
        assert!(c.cars().all(|car| car.pack.is_none()));
    }

    #[test]
    fn small_catalogs_for_tests() {
        let c = Catalog::from_json(
            r#"{"source":{"sha256":"00"},
                "cars":[{"id":1,"year":2000,"makeId":1,"make":"Make","model":"Model","mediaName":"MK_Model_00","classId":2,"pi":650,"topSpeedMps":70.0}],
                "classes":[{"id":2,"name":"B","maxPi":700}],
                "buckets":[{"carId":1,"bucket":5,"hero":true}],
                "tracks":[{"id":9,"mediaName":"Anthem","lengthM":1000,"reverse":false}]}"#,
        )
        .unwrap();
        assert_eq!(c.bucket_of(1), Some(5));
        assert_eq!(c.bucket_cars(5), &[1]);
        assert_eq!(c.track(9).unwrap().name("en"), "Anthem");
    }
}
