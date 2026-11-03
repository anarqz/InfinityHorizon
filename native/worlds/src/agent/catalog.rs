// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! The server-side game catalog (DEDICATED_SERVER.md §9; WORLDS_OFFICIAL.md §8: `gamedbRC.slt`,
//! mounted read-only, never in an image), read the way the launcher reads the player's own copy
//! (`DsCatalogs`, `RoadTripCatalog.Load`, `PartsCatalog` in Infinity.Core): the same queries, the
//! same bounds and the same `catalog.set` version, so an official world sees the rows a launcher
//! would send. Logs carry counts only, never a row.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// The most Road Trip events in one `catalog.set` (§4.2).
pub const MAX_EVENTS: usize = 1024;
/// The most classes per event (CarClassId 0-7).
pub const MAX_CLASSES: usize = 8;
/// The most row ids per slot table, and in all, in one `catalog.parts` (§4.2).
pub const MAX_IDS_PER_TABLE: usize = 1024;
pub const MAX_IDS: usize = 4096;
/// The Road Trip locations the game serves (§9).
pub const FIRST_LOCATION: i64 = 3;
pub const LAST_LOCATION: i64 = 9;
/// Models kept (the reference's cache clears when full).
pub const MAX_CACHED_MODELS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Car,
    Engine,
    Drivetrain,
    Body,
    Motor,
}

/// The player object's 43 part tables in order, with the column naming their owner (§9).
const SLOT_TABLES: [(&str, Scope, &str); 43] = [
    ("List_UpgradeEngine", Scope::Car, "Ordinal"),
    ("List_UpgradeDrivetrain", Scope::Car, "Ordinal"),
    ("List_UpgradeCarBody", Scope::Car, "Ordinal"),
    ("List_UpgradeMotor", Scope::Car, "Ordinal"),
    ("List_UpgradeBrakes", Scope::Car, "Ordinal"),
    ("List_UpgradeSpringDamper", Scope::Car, "Ordinal"),
    ("List_UpgradeAntiSwayFront", Scope::Car, "Ordinal"),
    ("List_UpgradeAntiSwayRear", Scope::Car, "Ordinal"),
    ("List_UpgradeTireCompound", Scope::Car, "Ordinal"),
    ("List_UpgradeRearWing", Scope::Car, "Ordinal"),
    ("List_UpgradeRimSizeFront", Scope::Car, "Ordinal"),
    ("List_UpgradeRimSizeRear", Scope::Car, "Ordinal"),
    ("List_UpgradeEngineCamshaft", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineValves", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineDisplacement", Scope::Engine, "EngineID"),
    (
        "List_UpgradeEnginePistonsCompression",
        Scope::Engine,
        "EngineID",
    ),
    ("List_UpgradeEngineFuelSystem", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineIgnition", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineExhaust", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineIntake", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineFlywheel", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineManifold", Scope::Engine, "EngineID"),
    (
        "List_UpgradeEngineRestrictorPlate",
        Scope::Engine,
        "EngineID",
    ),
    ("List_UpgradeEngineOilCooling", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineTurboSingle", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineTurboTwin", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineTurboQuad", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineCSC", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineDSC", Scope::Engine, "EngineID"),
    ("List_UpgradeEngineIntercooler", Scope::Engine, "EngineID"),
    (
        "List_UpgradeDrivetrainClutch",
        Scope::Drivetrain,
        "DrivetrainID",
    ),
    (
        "List_UpgradeDrivetrainTransmission",
        Scope::Drivetrain,
        "DrivetrainID",
    ),
    (
        "List_UpgradeDrivetrainDriveline",
        Scope::Drivetrain,
        "DrivetrainID",
    ),
    (
        "List_UpgradeDrivetrainDifferential",
        Scope::Drivetrain,
        "DrivetrainID",
    ),
    ("List_UpgradeCarBodyFrontBumper", Scope::Body, "CarBodyID"),
    ("List_UpgradeCarBodyRearBumper", Scope::Body, "CarBodyID"),
    ("List_UpgradeCarBodyHood", Scope::Body, "CarBodyID"),
    ("List_UpgradeCarBodySideSkirt", Scope::Body, "CarBodyID"),
    (
        "List_UpgradeCarBodyTireWidthFront",
        Scope::Body,
        "CarBodyID",
    ),
    ("List_UpgradeCarBodyTireWidthRear", Scope::Body, "CarBodyID"),
    ("List_UpgradeCarBodyWeight", Scope::Body, "CarBodyID"),
    (
        "List_UpgradeCarBodyChassisStiffness",
        Scope::Body,
        "CarBodyID",
    ),
    ("List_UpgradeMotorParts", Scope::Motor, "MotorID"),
];

/// The table whose rows of a model offer a scope's ids, and that id's column.
fn source(scope: Scope) -> (&'static str, &'static str) {
    match scope {
        Scope::Engine => ("List_UpgradeEngine", "EngineID"),
        Scope::Drivetrain => ("List_UpgradeDrivetrain", "DrivetrainID"),
        Scope::Body => ("List_UpgradeCarBody", "CarBodyID"),
        Scope::Motor | Scope::Car => ("List_UpgradeMotor", "MotorID"),
    }
}

/// One Road Trip event as `catalog.set` carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoadTripEvent {
    pub id: i64,
    pub location: i64,
    pub mode: i64,
    pub min_players: i64,
    pub classes: Vec<i64>,
}

/// The catalog, opened read-only for the agent's life.
pub struct Catalog {
    db: Connection,
    /// The per-model statement; `None` when the parts tables are missing (nothing is judged).
    parts_query: Option<String>,
    models: HashMap<u32, Option<BTreeMap<String, Vec<u32>>>>,
    broken: bool,
}

/// The catalog file could not be opened as a SQLite database.
#[derive(Debug)]
pub struct OpenError;

impl Catalog {
    /// Opens `path` read-only and immutable (a read-only mount has no room for a journal).
    pub fn open(path: &Path) -> Result<Catalog, OpenError> {
        let uri = format!("file:{}?immutable=1", uri_path(path).ok_or(OpenError)?);
        let db = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|_| OpenError)?;
        // a file that is not a database fails here, not at the first need
        db.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|_| OpenError)?;
        let parts_query = parts_tables_present(&db).then(parts_query);
        Ok(Catalog {
            db,
            parts_query,
            models: HashMap::new(),
            broken: false,
        })
    }

    /// Whether the parts rule has tables to read.
    pub fn has_parts(&self) -> bool {
        self.parts_query.is_some()
    }

    /// The Road Trip events at the served locations, in the database's order (`RoadTripCatalog.Load`);
    /// `None` when the tables cannot be read.
    pub fn road_trip(&self) -> Option<Vec<RoadTripEvent>> {
        let mut classes: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut st = self
            .db
            .prepare("SELECT AnthemOnlineEventId, CarClassId FROM AnthemOnlineEventPermissions")
            .ok()?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .ok()?;
        for row in rows {
            let (id, class) = row.ok()?;
            let set = classes.entry(id).or_default();
            if !set.contains(&class) {
                set.push(class);
            }
        }
        let mut st = self
            .db
            .prepare(
                "SELECT e.AnthemOnlineEventId, e.RoadTripLocationId, e.RaceModeId, m.MinimumPlayers \
                 FROM AnthemOnlineEvents e JOIN RaceModes m ON m.id = e.RaceModeId \
                 WHERE e.RoadTripLocationId > 0",
            )
            .ok()?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .ok()?;
        let mut events = Vec::new();
        for row in rows {
            let (id, location, mode, min) = row.ok()?;
            if !(FIRST_LOCATION..=LAST_LOCATION).contains(&location) {
                continue;
            }
            events.push(RoadTripEvent {
                id,
                location,
                mode,
                min_players: min.max(1),
                classes: classes.get(&id).cloned().unwrap_or_default(),
            });
        }
        Some(events)
    }

    /// The `catalog.parts` answer's `slots` for `model`: empty when the database does not have the
    /// model or cannot be read (§9: not judged).
    pub fn parts(&mut self, model: u32) -> BTreeMap<String, Vec<u32>> {
        if self.broken {
            return BTreeMap::new();
        }
        let Some(query) = self.parts_query.as_deref() else {
            return BTreeMap::new();
        };
        if let Some(known) = self.models.get(&model) {
            return known.clone().unwrap_or_default();
        }
        if self.models.len() >= MAX_CACHED_MODELS {
            self.models.clear();
        }
        match read_model(&self.db, query, model) {
            Ok(found) => {
                let slots = found.map(|by_slot| {
                    bound(
                        SLOT_TABLES
                            .iter()
                            .zip(by_slot)
                            .map(|((table, _, _), ids)| (*table, ids)),
                    )
                    .0
                });
                self.models.insert(model, slots.clone());
                slots.unwrap_or_default()
            }
            Err(()) => {
                self.broken = true;
                BTreeMap::new()
            }
        }
    }
}

fn read_model(db: &Connection, query: &str, model: u32) -> Result<Option<Vec<HashSet<u32>>>, ()> {
    let exists = db
        .prepare_cached("SELECT 1 FROM Data_Car WHERE Id = ?1")
        .and_then(|mut st| st.exists([i64::from(model)]))
        .map_err(|_| ())?;
    if !exists {
        return Ok(None);
    }
    let mut slots: Vec<HashSet<u32>> = vec![HashSet::new(); SLOT_TABLES.len()];
    let mut st = db.prepare_cached(query).map_err(|_| ())?;
    let rows = st
        .query_map([i64::from(model)], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .map_err(|_| ())?;
    for row in rows {
        let (slot, id) = row.map_err(|_| ())?;
        if let (Ok(slot), Ok(id)) = (usize::try_from(slot), u32::try_from(id))
            && id > 0
            && slot < slots.len()
        {
            slots[slot].insert(id);
        }
    }
    Ok(Some(slots))
}

/// One statement per model: every slot's rows of that model (the ordinal itself, or the scope ids
/// its own rows offer), `PartsCatalog`'s query.
fn parts_query() -> String {
    SLOT_TABLES
        .iter()
        .enumerate()
        .map(|(i, (table, scope, column))| {
            if *scope == Scope::Car {
                format!("SELECT {i}, Id FROM [{table}] WHERE [{column}] = ?1")
            } else {
                let (src, col) = source(*scope);
                format!(
                    "SELECT {i}, Id FROM [{table}] WHERE [{column}] IN (SELECT [{col}] FROM [{src}] WHERE Ordinal = ?1)"
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
}

/// Every table and column the parts query reads exists (`PartsCatalog.Load`'s check).
fn parts_tables_present(db: &Connection) -> bool {
    let mut needed: Vec<(&str, &str)> = vec![("Data_Car", "Id")];
    for (table, _, column) in SLOT_TABLES {
        needed.push((table, column));
        needed.push((table, "Id"));
    }
    for scope in [Scope::Engine, Scope::Drivetrain, Scope::Body, Scope::Motor] {
        let (table, column) = source(scope);
        needed.push((table, "Ordinal"));
        needed.push((table, column));
    }
    let mut columns: HashMap<&str, HashSet<String>> = HashMap::new();
    for (table, column) in needed {
        if !columns.contains_key(table) {
            let Ok(mut st) = db.prepare("SELECT name FROM pragma_table_info(?1)") else {
                return false;
            };
            let Ok(rows) = st.query_map([table], |r| r.get::<_, String>(0)) else {
                return false;
            };
            let set: HashSet<String> = rows
                .filter_map(Result::ok)
                .map(|c| c.to_ascii_lowercase())
                .collect();
            columns.insert(table, set);
        }
        if !columns[table].contains(&column.to_ascii_lowercase()) {
            return false;
        }
    }
    true
}

/// §4.2's bounds without ever truncating a list (a partial list would flag legitimate parts): a
/// table past [`MAX_IDS_PER_TABLE`] is left out, and once [`MAX_IDS`] would be passed every
/// remaining table is left out (`DsCatalogs.Bound`). Returns the tables and how many were left out.
pub fn bound<'a>(
    tables: impl IntoIterator<Item = (&'a str, HashSet<u32>)>,
) -> (BTreeMap<String, Vec<u32>>, usize) {
    let mut slots = BTreeMap::new();
    let (mut total, mut omitted, mut full) = (0usize, 0usize, false);
    for (table, rows) in tables {
        let mut ids: Vec<u32> = rows.into_iter().collect();
        ids.sort_unstable();
        if full || ids.len() > MAX_IDS_PER_TABLE {
            omitted += 1;
            continue;
        }
        if total + ids.len() > MAX_IDS {
            full = true;
            omitted += 1;
            continue;
        }
        total += ids.len();
        slots.insert(table.to_string(), ids);
    }
    (slots, omitted)
}

/// The `catalog.set` request's fields for these events (`DsCatalogs.RoadTrip`): at most
/// [`MAX_EVENTS`], classes 0-7 distinct (at most 8), versioned by their content.
pub fn catalog_set(events: &[RoadTripEvent]) -> Map<String, Value> {
    let mut text = String::new();
    let mut rows = Vec::new();
    for e in events.iter().take(MAX_EVENTS) {
        let mut classes: Vec<i64> = Vec::new();
        for c in &e.classes {
            if (0..MAX_CLASSES as i64).contains(c)
                && !classes.contains(c)
                && classes.len() < MAX_CLASSES
            {
                classes.push(*c);
            }
        }
        let joined = classes
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(".");
        text.push_str(&format!(
            "{}:{}:{}:{}:{joined};",
            e.id, e.location, e.mode, e.min_players
        ));
        rows.push(json!({"id": e.id, "location": e.location, "mode": e.mode,
            "minPlayers": e.min_players, "classes": classes}));
    }
    let version = format!(
        "rt-{}",
        &super::sign::hex(&Sha256::digest(text.as_bytes()))[..16]
    );
    let mut m = Map::new();
    m.insert("game".into(), json!(super::config::GAME));
    m.insert("version".into(), json!(version));
    m.insert("roadTrip".into(), json!({"events": rows}));
    m
}

/// The path as a SQLite URI path (`%`, `?` and `#` escaped); `None` for a path that is not UTF-8.
fn uri_path(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '%' => out.push_str("%25"),
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic catalog (made-up ids, no game data): the tables the queries read.
    fn synthetic(dir: &Path, with_parts: bool) -> std::path::PathBuf {
        let path = dir.join("catalog.slt");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE RaceModes (id INTEGER, MinimumPlayers INTEGER);
             INSERT INTO RaceModes VALUES (1, 0), (2, 4);
             CREATE TABLE AnthemOnlineEvents (AnthemOnlineEventId INTEGER, RoadTripLocationId INTEGER, RaceModeId INTEGER);
             INSERT INTO AnthemOnlineEvents VALUES (101, 3, 1), (102, 0, 1), (103, 9, 2), (104, 10, 1), (105, 4, 2);
             CREATE TABLE AnthemOnlineEventPermissions (AnthemOnlineEventId INTEGER, CarClassId INTEGER);
             INSERT INTO AnthemOnlineEventPermissions VALUES (101, 2), (101, 0), (101, 2), (103, 9), (103, 5);",
        )
        .unwrap();
        if with_parts {
            db.execute_batch(
                "CREATE TABLE Data_Car (Id INTEGER); INSERT INTO Data_Car VALUES (500), (501);",
            )
            .unwrap();
            for (table, scope, column) in SLOT_TABLES {
                let extra = match (scope, table) {
                    (Scope::Car, "List_UpgradeEngine") => ", EngineID INTEGER",
                    (Scope::Car, "List_UpgradeDrivetrain") => ", DrivetrainID INTEGER",
                    (Scope::Car, "List_UpgradeCarBody") => ", CarBodyID INTEGER",
                    (Scope::Car, "List_UpgradeMotor") => ", MotorID INTEGER",
                    _ => "",
                };
                db.execute_batch(&format!(
                    "CREATE TABLE [{table}] (Id INTEGER, [{column}] INTEGER{extra});"
                ))
                .unwrap();
            }
            db.execute_batch(
                "INSERT INTO List_UpgradeEngine VALUES (11, 500, 70), (12, 500, 71), (13, 501, 72);
                 INSERT INTO List_UpgradeBrakes VALUES (31, 500), (32, 500), (0, 500), (33, 501);
                 INSERT INTO List_UpgradeEngineCamshaft VALUES (41, 70), (42, 71), (43, 72);
                 INSERT INTO List_UpgradeMotor VALUES (51, 500, 90);
                 INSERT INTO List_UpgradeMotorParts VALUES (61, 90), (62, 91);",
            )
            .unwrap();
            // a table past the per-table bound is omitted, never truncated
            let mut big = String::from("INSERT INTO List_UpgradeRearWing VALUES ");
            big.push_str(
                &(1..=1025)
                    .map(|i| format!("({}, 500)", 1000 + i))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            db.execute_batch(&big).unwrap();
        }
        path
    }

    fn temp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fi-agent-cat-{}", super::super::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn road_trip_rows_and_their_version_follow_the_launcher() {
        let dir = temp();
        let cat = Catalog::open(&synthetic(&dir, false)).unwrap();
        assert!(!cat.has_parts());
        let events = cat.road_trip().unwrap();
        assert_eq!(
            events,
            vec![
                RoadTripEvent {
                    id: 101,
                    location: 3,
                    mode: 1,
                    min_players: 1,
                    classes: vec![2, 0]
                },
                RoadTripEvent {
                    id: 103,
                    location: 9,
                    mode: 2,
                    min_players: 4,
                    classes: vec![9, 5]
                },
                RoadTripEvent {
                    id: 105,
                    location: 4,
                    mode: 2,
                    min_players: 4,
                    classes: vec![]
                },
            ]
        );
        let set = catalog_set(&events);
        let rows = &set["roadTrip"]["events"];
        assert_eq!(
            rows[1]["classes"],
            json!([5]),
            "a class outside 0-7 is dropped"
        );
        assert_eq!(rows[0]["minPlayers"], 1);
        let text = "101:3:1:1:2.0;103:9:2:4:5;105:4:2:4:;";
        let want = format!(
            "rt-{}",
            &super::super::sign::hex(&Sha256::digest(text.as_bytes()))[..16]
        );
        assert_eq!(set["version"], json!(want));
        assert_eq!(set["game"], "fh2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parts_of_a_model_by_the_reference_joins() {
        let dir = temp();
        let mut cat = Catalog::open(&synthetic(&dir, true)).unwrap();
        assert!(cat.has_parts());
        let slots = cat.parts(500);
        assert_eq!(slots["List_UpgradeEngine"], vec![11, 12]);
        assert_eq!(slots["List_UpgradeBrakes"], vec![31, 32], "id 0 is no part");
        assert_eq!(slots["List_UpgradeEngineCamshaft"], vec![41, 42]);
        assert_eq!(slots["List_UpgradeMotorParts"], vec![61]);
        assert!(
            !slots.contains_key("List_UpgradeRearWing"),
            "omitted past 1024"
        );
        assert_eq!(slots["List_UpgradeEngineValves"], Vec::<u32>::new());
        assert_eq!(cat.parts(501)["List_UpgradeEngine"], vec![13]);
        assert!(
            cat.parts(999).is_empty(),
            "a model the database does not have"
        );
        assert!(cat.parts(500).contains_key("List_UpgradeEngine"), "cached");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_bounds_omit_whole_tables() {
        let t = |n: u32, from: u32| (from..from + n).collect::<HashSet<u32>>();
        let (slots, omitted) = bound([
            ("a", t(1000, 1)),
            ("b", t(1025, 1)),
            ("c", t(1000, 5000)),
            ("d", t(1000, 9000)),
            ("e", t(1000, 20000)),
            ("f", t(97, 30000)),
            ("g", t(1, 40000)),
        ]);
        assert_eq!(
            omitted, 3,
            "b past 1024; f would pass 4096, so f and every later table"
        );
        assert_eq!(
            slots.keys().map(String::as_str).collect::<Vec<_>>(),
            ["a", "c", "d", "e"]
        );
        assert_eq!(slots["a"].first(), Some(&1));
        let (slots, omitted) = bound([
            ("a", t(1024, 1)),
            ("b", t(1024, 1)),
            ("c", t(1024, 1)),
            ("d", t(1024, 1)),
            ("e", t(1, 1)),
        ]);
        assert_eq!((slots.len(), omitted), (4, 1), "exactly 4096 fits");
    }

    #[test]
    fn a_file_that_is_not_a_database_is_refused() {
        let dir = temp();
        let path = dir.join("x.slt");
        std::fs::write(
            &path,
            b"not a database, just bytes long enough to look like a header",
        )
        .unwrap();
        assert!(Catalog::open(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
