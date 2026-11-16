//! FH2's PlayerDatabase, read on the server for the Hardcore conservation checks (OD-62 step 2,
//! OD-67, ADR 0021; CLOUD_SAVES.md §7.3; the fields: `re/hardcore-save-fields.md` §3).
//!
//! `data/PlayerDatabase` is the game's `ssmc` container (`"ssmc"`, `u32` LE inner length) around a
//! SQLite 3 file (1 KiB pages, rollback journal, no indexes). It holds what ProfileData does not:
//! the garage (`Career_Garage`, with each car's origin in `Flags`), unlocked perks, purchased
//! parts, car vouchers, barn finds and claimed event cars. [`read`] returns exactly those facts.
//!
//! The file is **untrusted input** (any process can write a save), so this is not a SQLite engine
//! but a minimal, read-only reader of six pinned tables ([`schema`]), owner-chosen over a bundled
//! C SQLite (OD-67):
//! - pure Rust, `unsafe_code = "forbid"`, std only, no SQL: it walks `sqlite_schema` and the six
//!   table b-trees and nothing else;
//! - hard limits: [`sqlite::MAX_DATABASE`] bytes, [`sqlite::MAX_PAGES_READ`] pages (each page at
//!   most once, so a cycle refuses instead of looping), [`sqlite::MAX_DEPTH`],
//!   [`sqlite::MAX_PAYLOAD`] per record, assembled record bytes at most
//!   [`sqlite::MAX_ASSEMBLED_PER_BYTE`] times the file, distinct cells per page, each table's own
//!   row cap and [`schema::MAX_SCHEMA_ROWS`];
//! - every failure is an [`Unreadable`] with a stable code; nothing panics on any input;
//! - pinned DDL: each table's stored `CREATE TABLE` must be the game's, byte for byte
//!   ([`struct@schema::DDL`], the game data's), so columns are read by position only where the positions are known;
//! - integer columns only: a TEXT or BLOB column is skipped by its length and never decoded, and
//!   the page and record types are crate-private, so `Career_Garage.OriginalOwner` (a gamertag,
//!   personal data) cannot leave this crate.
//!
//! Server only: `fi-saves` builds into the launcher and does not depend on this crate.

pub mod schema;
pub mod sqlite;

use schema::{BARN_FINDS, EVENT_CARS_CLAIMED, FREE_CARS, GARAGE, PERKS, PURCHASED_PARTS, Table};
use sqlite::{File, Record};
use std::fmt;

/// The `ssmc` container's tag.
pub const CONTAINER_TAG: &[u8; 4] = b"ssmc";

/// What the server reads of one PlayerDatabase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlayerDb {
    /// `Career_Garage`, in garage-id order.
    pub garage: Vec<GarageCar>,
    /// `PerkUnlock.UnlockId` (gamedb `Perks.Id`), ascending.
    pub perks: Vec<i64>,
    /// `Career_PurchasedParts`, in storage order.
    pub purchased_parts: Vec<PurchasedPart>,
    /// `FreeCars`, by car id.
    pub free_cars: Vec<FreeCar>,
    /// `BarnFinds`, by car ordinal.
    pub barn_finds: Vec<BarnFind>,
    /// `Career_EventCarsClaimed.RewardsEventCarsId`, ascending.
    pub event_cars_claimed: Vec<i64>,
    /// Pages the read visited (out of [`sqlite::MAX_PAGES_READ`]).
    pub pages_read: u32,
}

/// One owned car. No text column: `OriginalOwner`, file names and the thumbnail are never read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GarageCar {
    /// `Id`, the garage id (the rowid).
    pub id: i64,
    /// `CarId`, the gamedb car ordinal.
    pub car_id: Option<i64>,
    /// `Flags` (bits: [`GarageCar::origin`]).
    pub flags: Option<i64>,
    /// `PartsValue`, the sum of the car's `PricePaid` when the game keeps it consistent.
    pub parts_value: Option<i64>,
}

/// How a garage car was acquired, from its `Flags` (RE §3.2). `0x08` (stock) and `0x10` (a UI
/// toggle) say nothing about origin and are ignored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// No origin bit: bought with credits or a `FreeCars` voucher (or the starter pick).
    Bought,
    /// `0x02`: an event, championship or level reward.
    Awarded,
    /// `0x01`: a barn find, a unicorn model or a Supporter car.
    Special,
    /// `0x20` or `0x40`: a gift.
    Gifted,
}

impl GarageCar {
    /// The car's origin; a NULL `Flags` reads as no bit set.
    pub fn origin(&self) -> Origin {
        let flags = self.flags.unwrap_or(0);
        if flags & 0x60 != 0 {
            Origin::Gifted
        } else if flags & 0x02 != 0 {
            Origin::Awarded
        } else if flags & 0x01 != 0 {
            Origin::Special
        } else {
            Origin::Bought
        }
    }
}

/// One purchased part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PurchasedPart {
    /// `GarageId`, the car it is fitted to.
    pub garage_id: Option<i64>,
    /// `PartEnum`, the upgrade slot.
    pub part_enum: Option<i64>,
    /// `PartId`, the gamedb part.
    pub part_id: Option<i64>,
    /// `PricePaid`.
    pub price_paid: Option<i64>,
}

/// One car voucher.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreeCar {
    /// `CarId` (the rowid).
    pub car_id: i64,
    /// `FreeCount`, vouchers left.
    pub free_count: Option<i64>,
}

/// One barn find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarnFind {
    /// `CarOrdinal` (the rowid).
    pub car_ordinal: i64,
    /// `State`.
    pub state: Option<i64>,
}

/// Why a PlayerDatabase could not be read. Never evidence of cheating on its own: in shadow it is
/// recorded and the save goes on (`bundle::verify`'s container check stays the only refusal). The
/// messages carry page numbers and the game's table and column names, never a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// Not an `ssmc` container around a SQLite file, or the inner length does not fit.
    NotGameDatabase,
    /// More than [`sqlite::MAX_DATABASE`] bytes.
    TooLarge,
    /// A database header field the reader does not accept (the field's name).
    Header(&'static str),
    /// A page number outside the file, or a page that is not a table b-tree page.
    Page(u32),
    /// A page reached a second time (a cycle, or two trees sharing a page).
    PageReused(u32),
    /// More than [`sqlite::MAX_PAGES_READ`] pages.
    Budget,
    /// A b-tree deeper than [`sqlite::MAX_DEPTH`].
    Depth,
    /// A record larger than [`sqlite::MAX_PAYLOAD`] or with a broken header.
    Payload,
    /// An overflow chain that ends early (the leaf page it started from).
    Record(u32),
    /// A pinned table is not in `sqlite_schema`.
    MissingTable(&'static str),
    /// A pinned table's DDL is not the one the reader knows (or appears twice).
    Schema(&'static str),
    /// A pinned table has more rows than its cap.
    TooManyRows(&'static str),
    /// A column the reader needs holds a REAL, TEXT or BLOB.
    Type(&'static str, &'static str),
}

impl Unreadable {
    /// A stable code for logs and assessment details.
    pub fn code(&self) -> &'static str {
        match self {
            Unreadable::NotGameDatabase => "NOT_GAME_DATABASE",
            Unreadable::TooLarge => "TOO_LARGE",
            Unreadable::Header(_) => "HEADER",
            Unreadable::Page(_) => "PAGE",
            Unreadable::PageReused(_) => "PAGE_REUSED",
            Unreadable::Budget => "PAGE_BUDGET",
            Unreadable::Depth => "DEPTH",
            Unreadable::Payload => "PAYLOAD",
            Unreadable::Record(_) => "OVERFLOW",
            Unreadable::MissingTable(_) => "MISSING_TABLE",
            Unreadable::Schema(_) => "SCHEMA",
            Unreadable::TooManyRows(_) => "TOO_MANY_ROWS",
            Unreadable::Type(_, _) => "COLUMN_TYPE",
        }
    }
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unreadable::NotGameDatabase => f.write_str("not an ssmc container around SQLite"),
            Unreadable::TooLarge => f.write_str("database larger than the reader's limit"),
            Unreadable::Header(field) => write!(f, "database header: unsupported {field}"),
            Unreadable::Page(n) => write!(f, "page {n} is not a readable table page"),
            Unreadable::PageReused(n) => write!(f, "page {n} reached twice"),
            Unreadable::Budget => f.write_str("page budget exhausted"),
            Unreadable::Depth => f.write_str("b-tree deeper than the reader's limit"),
            Unreadable::Payload => f.write_str("record too large or malformed"),
            Unreadable::Record(n) => write!(f, "overflow chain from page {n} ends early"),
            Unreadable::MissingTable(t) => write!(f, "table {t} missing"),
            Unreadable::Schema(t) => write!(f, "table {t} has an unknown schema"),
            Unreadable::TooManyRows(t) => write!(f, "table {t} has more rows than its cap"),
            Unreadable::Type(t, c) => write!(f, "{t}.{c} is not an integer"),
        }
    }
}

impl std::error::Error for Unreadable {}

/// Reads `data/PlayerDatabase` as the save holds it: the `ssmc` container, then the SQLite file.
pub fn read(player_database: &[u8]) -> Result<PlayerDb, Unreadable> {
    let inner = player_database
        .get(4..8)
        .filter(|_| player_database.get(..4) == Some(CONTAINER_TAG.as_slice()))
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .and_then(|len| player_database.get(8..8usize.checked_add(len)?))
        .ok_or(Unreadable::NotGameDatabase)?;
    read_sqlite(inner)
}

/// Reads the pinned tables of a bare SQLite file.
pub fn read_sqlite(database: &[u8]) -> Result<PlayerDb, Unreadable> {
    let mut file = File::open(database)?;
    let roots = roots(&mut file)?;
    let root = |t: &Table| {
        roots[schema::TABLES
            .iter()
            .position(|p| p.name == t.name)
            .unwrap_or(0)]
    };
    let mut db = PlayerDb::default();

    let (flags, car, parts) = (
        GARAGE.at("Flags"),
        GARAGE.at("CarId"),
        GARAGE.at("PartsValue"),
    );
    rows(&mut file, &GARAGE, root(&GARAGE), |rowid, r| {
        db.garage.push(GarageCar {
            id: rowid,
            car_id: int(r, &GARAGE, car)?,
            flags: int(r, &GARAGE, flags)?,
            parts_value: int(r, &GARAGE, parts)?,
        });
        Ok(())
    })?;
    rows(&mut file, &PERKS, root(&PERKS), |rowid, _| {
        db.perks.push(rowid);
        Ok(())
    })?;
    let p = &PURCHASED_PARTS;
    let cols = (
        p.at("GarageId"),
        p.at("PartEnum"),
        p.at("PartId"),
        p.at("PricePaid"),
    );
    rows(&mut file, p, root(p), |_, r| {
        db.purchased_parts.push(PurchasedPart {
            garage_id: int(r, p, cols.0)?,
            part_enum: int(r, p, cols.1)?,
            part_id: int(r, p, cols.2)?,
            price_paid: int(r, p, cols.3)?,
        });
        Ok(())
    })?;
    let count = FREE_CARS.at("FreeCount");
    rows(&mut file, &FREE_CARS, root(&FREE_CARS), |rowid, r| {
        db.free_cars.push(FreeCar {
            car_id: rowid,
            free_count: int(r, &FREE_CARS, count)?,
        });
        Ok(())
    })?;
    let state = BARN_FINDS.at("State");
    rows(&mut file, &BARN_FINDS, root(&BARN_FINDS), |rowid, r| {
        db.barn_finds.push(BarnFind {
            car_ordinal: rowid,
            state: int(r, &BARN_FINDS, state)?,
        });
        Ok(())
    })?;
    rows(
        &mut file,
        &EVENT_CARS_CLAIMED,
        root(&EVENT_CARS_CLAIMED),
        |rowid, _| {
            db.event_cars_claimed.push(rowid);
            Ok(())
        },
    )?;
    db.pages_read = file.pages_read();
    Ok(db)
}

/// The root page of every pinned table, in [`schema::TABLES`] order, from `sqlite_schema` (page 1:
/// `type`, `name`, `tbl_name`, `rootpage`, `sql`), each DDL checked against its pin.
fn roots(file: &mut File<'_>) -> Result<[u32; schema::TABLES.len()], Unreadable> {
    let mut roots = [0u32; schema::TABLES.len()];
    let mut rows = 0usize;
    file.scan(1, |_, payload| {
        rows += 1;
        if rows > schema::MAX_SCHEMA_ROWS {
            return Err(Unreadable::TooManyRows("sqlite_schema"));
        }
        let r = Record::parse(payload)?;
        if r.text(0) != Some("table") {
            return Ok(());
        }
        let Some(i) = r
            .text(1)
            .and_then(|name| schema::TABLES.iter().position(|t| t.name == name))
        else {
            return Ok(());
        };
        let table = schema::TABLES[i];
        let root = r.int(3).ok().flatten().and_then(|n| u32::try_from(n).ok());
        let sql = r.text(4).unwrap_or_default();
        if roots[i] != 0 || !table.matches(sql) {
            return Err(Unreadable::Schema(table.name));
        }
        roots[i] = root
            .filter(|n| *n > 1)
            .ok_or(Unreadable::Schema(table.name))?;
        Ok(())
    })?;
    if let Some(i) = roots.iter().position(|r| *r == 0) {
        return Err(Unreadable::MissingTable(schema::TABLES[i].name));
    }
    Ok(roots)
}

/// Every row of one pinned table, within its row cap.
fn rows(
    file: &mut File<'_>,
    table: &'static Table,
    root: u32,
    mut row: impl FnMut(i64, &Record<'_>) -> Result<(), Unreadable>,
) -> Result<(), Unreadable> {
    let mut n = 0usize;
    file.scan(root, |rowid, payload| {
        n += 1;
        if n > table.max_rows {
            return Err(Unreadable::TooManyRows(table.name));
        }
        row(rowid, &Record::parse(payload)?)
    })
}

fn int(r: &Record<'_>, table: &'static Table, column: usize) -> Result<Option<i64>, Unreadable> {
    r.int(column)
        .map_err(|()| Unreadable::Type(table.name, table.columns[column]))
}
