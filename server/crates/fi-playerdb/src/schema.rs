//! The tables the reader pins: their names, every column in the game's order, and the rowid alias.
//! A table whose stored DDL names other columns, or in another order, is refused
//! ([`crate::Unreadable::Schema`]): the reader reads columns by position, so a schema it does not
//! know is never guessed at.

/// One pinned table.
pub struct Table {
    /// Its name in `sqlite_schema`.
    pub name: &'static str,
    /// Every column, in the DDL's order.
    pub columns: &'static [&'static str],
    /// The `INTEGER PRIMARY KEY` column, stored as NULL in the record (its value is the rowid).
    pub key: Option<&'static str>,
    /// The most rows read; more refuse ([`crate::Unreadable::TooManyRows`]).
    pub max_rows: usize,
}

impl Table {
    /// The position of `column` (a name from this table's own list).
    pub fn at(&self, column: &str) -> usize {
        self.columns
            .iter()
            .position(|c| *c == column)
            .expect("a pinned column")
    }

    /// Whether `sql` is this table's DDL as the game creates it ([`struct@DDL`]), word for word. SQLite
    /// stores the `CREATE TABLE` text verbatim, so anything else (another column, another type, a
    /// `DESC` key that is no rowid alias) is a schema the reader does not know. Only the runs of
    /// whitespace between words may differ: the game pads the table name with a varying number of
    /// spaces (a real Azimuth save on 2026-10-27 had 17 after `[BarnFinds]`, the fixtures 3), which
    /// changes no column, type or key.
    pub fn matches(&self, sql: &str) -> bool {
        ddl(self.name).is_some_and(|pin| {
            pin.split_ascii_whitespace()
                .eq(sql.split_ascii_whitespace())
        })
    }
}

/// The game data file of the pinned tables' DDL (`FI_GAME_DATA_DIR`; never in the crate).
pub const DDL_FILE: &str = "fh2/playerdb-schema.sql";

/// The game's own DDL of the pinned tables (and `AddOnState`, which the fixtures create first),
/// one `CREATE TABLE ...;` per statement, as `sqlite_schema.sql` stores it: the game data's
/// [`DDL_FILE`], read once (a save is never judged against a missing DDL: the process checks it at
/// start).
pub static DDL: fi_gamedata::Lazy<String> =
    fi_gamedata::Lazy::new(|| fi_gamedata::parse(DDL_FILE, |t| Ok::<_, String>(t.to_owned())));

/// The stored DDL of table `name`, without its `;`.
fn ddl(name: &str) -> Option<&'static str> {
    let head = format!("CREATE TABLE [{name}]");
    DDL.split(";\n")
        .find(|stmt| stmt.starts_with(&head) && stmt[head.len()..].starts_with(' '))
}

/// `Career_Garage`: one row per owned car (the game caps the garage at 550, RE §3.2).
#[rustfmt::skip]
pub const GARAGE: Table = Table {
    name: "Career_Garage",
    columns: &[
        "Id", "CarId", "PerformanceIndex", "ClassID", "PartsValue", "SpeedRating",
        "AccelerationRating", "LaunchRating", "BrakingRating", "HandlingRating", "CurbWeight",
        "WeightDistribution", "AspirationTypeId", "SimPeakPower", "SimPeakAngVel", "SimPeakTorque",
        "SimPeakTorqueAngVel", "SimRedlineAngVel", "PeakIntakePSI", "TopSpeed", "DistanceDriven",
        "TimeDriven", "TotalWinnings", "TotalRepairs", "NumVictories", "NumPodiums", "NumRaces",
        "NumOwners", "NumTimesSold", "TimeDrivenInRoadTrips", "CurOwnerNumRaces",
        "CurOwnerWinnings", "NumSkillPointsEarned", "OriginalOwner", "CarGroup", "Flags", "Engine",
        "Drivetrain", "CarBody", "Motor", "Brakes", "SpringDamper", "AntiSwayFront",
        "AntiSwayRear", "TireCompound", "RearWing", "RimSizeFront", "RimSizeRear", "Camshaft",
        "Valves", "Displacement", "PistonsCompression", "FuelSystem", "Ignition", "Exhaust",
        "Intake", "Flywheel", "Manifold", "RestrictorPlate", "OilCooling", "SingleTurbo",
        "TwinTurbo", "QuadTurbo", "SuperchargerCSC", "SuperchargerDSC", "Intercooler", "Clutch",
        "Transmission", "Driveline", "Differential", "FrontBumper", "RearBumper", "Hood",
        "SideSkirts", "TireWidthFront", "TireWidthRear", "WeightReduction", "ChassisStiffness",
        "MotorParts", "TireBrand", "WheelStyle", "DefaultManufacturerColorIndex", "LiveryFileName",
        "TuneFileName", "VersionedTuneId", "VersionedLiveryId", "Guid", "Thumbnail",
        "Tuning_frontTirePressure", "Tuning_rearTirePressure", "Tuning_finalDriveRatio",
        "Tuning_firstGear", "Tuning_secondGear", "Tuning_thirdGear", "Tuning_fourthGear",
        "Tuning_fifthGear", "Tuning_sixthGear", "Tuning_seventhGear", "Tuning_eighthGear",
        "Tuning_frontCamber", "Tuning_rearCamber", "Tuning_frontToe", "Tuning_rearToe",
        "Tuning_frontCaster", "Tuning_frontSwaybar", "Tuning_rearSwaybar", "Tuning_frontSpring",
        "Tuning_rearSpring", "Tuning_frontRideHeight", "Tuning_rearRideHeight",
        "Tuning_frontDampingStiffness", "Tuning_rearDampingStiffness", "Tuning_frontBumpRatio",
        "Tuning_rearBumpRatio", "Tuning_frontDownforce", "Tuning_rearDownforce",
        "Tuning_brakeBalance", "Tuning_brakePressure", "Tuning_frontAccel", "Tuning_rearAccel",
        "Tuning_frontDecel", "Tuning_rearDecel", "Tuning_centerTorque", "SharedID",
    ],
    key: Some("Id"),
    max_rows: 550,
};

/// `PerkUnlock`: one row per unlocked perk (25 perks and the row 0 "XP Everywhere").
pub const PERKS: Table = Table {
    name: "PerkUnlock",
    columns: &["UnlockId"],
    key: Some("UnlockId"),
    max_rows: 64,
};

/// `Career_PurchasedParts`: parts bought per garage car (43 upgrade slots per car).
pub const PURCHASED_PARTS: Table = Table {
    name: "Career_PurchasedParts",
    columns: &[
        "GarageId",
        "PartEnum",
        "PartId",
        "ManufacturerId",
        "PricePaid",
    ],
    key: None,
    max_rows: 550 * 43,
};

/// `FreeCars`: car vouchers, minted only from our economy service's offers (RE §3.3).
pub const FREE_CARS: Table = Table {
    name: "FreeCars",
    columns: &["CarId", "FreeCount"],
    key: Some("CarId"),
    max_rows: 1_024,
};

/// `BarnFinds`: barn-find progress (11 cars in every save seen).
pub const BARN_FINDS: Table = Table {
    name: "BarnFinds",
    columns: &["CarOrdinal", "State"],
    key: Some("CarOrdinal"),
    max_rows: 64,
};

/// `Career_EventCarsClaimed`: reward cars claimed from events.
pub const EVENT_CARS_CLAIMED: Table = Table {
    name: "Career_EventCarsClaimed",
    columns: &["RewardsEventCarsId"],
    key: Some("RewardsEventCarsId"),
    max_rows: 1_024,
};

/// The most `sqlite_schema` rows read (the game's database has 43 tables and no index).
pub const MAX_SCHEMA_ROWS: usize = 256;

/// Every pinned table, in the order they are read.
pub const TABLES: [&Table; 6] = [
    &GARAGE,
    &PERKS,
    &PURCHASED_PARTS,
    &FREE_CARS,
    &BARN_FINDS,
    &EVENT_CARS_CLAIMED,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The bracketed names of a DDL statement: the table, then its columns.
    fn names(sql: &str) -> Vec<&str> {
        sql.split('[')
            .skip(1)
            .filter_map(|p| p.split(']').next())
            .collect()
    }

    #[test]
    fn every_pin_is_its_ddl() {
        for table in TABLES {
            let sql = ddl(table.name).expect("pinned DDL");
            assert!(table.matches(sql));
            let names = names(sql);
            assert_eq!(names[0], table.name);
            assert_eq!(names[1..], *table.columns, "{}", table.name);
            if let Some(key) = table.key {
                assert_eq!(table.columns[0], key);
                assert!(sql.contains(&format!("[{key}] INTEGER PRIMARY KEY \n")));
            }
            assert!(!sql.contains("DESC") && !sql.contains("DEFAULT"));
        }
    }

    #[test]
    fn anything_but_the_exact_ddl_is_refused() {
        let sql = ddl(GARAGE.name).unwrap();
        assert!(!GARAGE.matches(&sql.replace(
            "[Id] INTEGER PRIMARY KEY ",
            "[Id] INTEGER PRIMARY KEY, Bogus INT "
        )));
        assert!(!GARAGE.matches(&sql.replace("PRIMARY KEY ", "PRIMARY KEY DESC ")));
        assert!(!GARAGE.matches(&sql.replace("[Id] INTEGER", "[Id]INTEGER")));
        assert!(!GARAGE.matches(&format!("{sql},[Bogus] INT")));
        assert!(!PERKS.matches(sql));
    }

    /// The game pads the table name with any number of spaces (a real save: 17 after
    /// `[BarnFinds]`); whitespace between words never changes a column, a type or the key.
    #[test]
    fn whitespace_between_words_may_differ() {
        for table in TABLES {
            let sql = ddl(table.name).unwrap();
            let head = format!("CREATE TABLE [{}]", table.name);
            let padded = sql.replacen(&head, &format!("{head}{}", " ".repeat(14)), 1);
            assert!(table.matches(&padded), "{}", table.name);
            assert!(table.matches(&format!("{sql} ")));
            assert!(table.matches(&sql.replace('\n', "\r\n")));
        }
    }
}
