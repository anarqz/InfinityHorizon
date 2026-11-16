//! The play dashboard's internal reads (DASHBOARD_API.md, CR-S82, OD-168): `/internal/dashboard/v1`
//! on the platform's second listener, read only by `apps/play`'s server. Wire shapes only; the
//! queries live in `fi_db::dashboard`, the handlers in `fi_surfaces::dashboard`. The TypeScript
//! mirror is `packages/contracts/src/dashboard.ts`.
//!
//! No shape here carries an XUID, an account id, a Discord user id, an e-mail or an address
//! (DASHBOARD_API.md §1): people are keyed by their plain name, lobbies by `worldId`.

use serde::{Deserialize, Serialize};

/// One count per mode (the same shape as `/core/v1/status`'s).
pub use crate::core::ModeCounts;

/// The header every request carries (constant-time compared with `FI_DASHBOARD_KEY`).
pub const KEY_HEADER: &str = crate::headers::FI_DASHBOARD_KEY;
/// `GET /lobbies` without `limit`.
pub const LOBBIES_DEFAULT_LIMIT: u32 = 50;
/// The largest `limit` of `GET /lobbies`.
pub const LOBBIES_MAX_LIMIT: u32 = 100;

/// `Live.lobbies`: the lobbies `GET /lobbies` can list (open, unexpired, audience `everyone`, no
/// allow-list, not in the pineapple pool), full ones included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveLobbies {
    /// Every listed lobby of the game.
    pub open: u64,
    /// Per mode; an `anyMode` lobby counts in both.
    pub per_mode: ModeCounts,
}

/// `GET /live`: the game's live aggregates.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Live {
    /// When read (RFC 3339 UTC, the database clock).
    pub at: String,
    /// The game (`fh2`).
    pub game: String,
    /// Admitted, unexpired, non-dedicated play sessions.
    pub online: u64,
    /// Of `online`, per profile mode.
    pub per_mode: ModeCounts,
    /// The listed lobbies.
    pub lobbies: LiveLobbies,
    /// The game's online capacity (`null`: unlimited).
    pub capacity: Option<i32>,
}

/// `Lobby.host`: the host's public card. Absent when the host is not listed on the dashboard
/// (hidden, banned, inactive, `social_hidden_at` or `web_listed = false`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LobbyHost {
    /// The plain name (OD-120).
    pub name: String,
    /// The linked Discord account's global name, else its username (DB-Q2); absent when unlinked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_name: Option<String>,
    /// A Discord avatar exists (the site builds `/img/avatar/<name>.webp`).
    pub avatar: bool,
}

/// One lobby of `GET /lobbies`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lobby {
    /// The world id (lowercase UUID; the launcher's `infinity-horizon://join/<worldId>`).
    pub world_id: String,
    /// The world's name.
    pub name: String,
    /// The host's card, when listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<LobbyHost>,
    /// Seated players (the host counts unless the world is dedicated).
    pub players: i32,
    /// Seats.
    pub max_players: i32,
    /// No seat to offer (seated, reserved and invited seats fill it): listed greyed out (DB-Q7).
    pub full: bool,
    /// `freeroam` | `roadtrip`.
    pub activity: String,
    /// The host's coarse region, when located.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// A modded world (display only: any mod set joins, T-1110-NOMODMATCH).
    pub mods: bool,
    /// A dedicated world.
    pub dedicated: bool,
    /// Open to Hardcore and Noob (OD-141).
    pub any_mode: bool,
    /// When it opened (RFC 3339 UTC).
    pub created_at: String,
}

/// `GET /lobbies`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LobbyList {
    /// Not full first, then most players, then newest.
    pub items: Vec<Lobby>,
}

/// Rows per page of a board (DASHBOARD_API.md §1, §3).
pub const BOARDS_PAGE_SIZE: u32 = 25;
/// The last page of a board (rank 1000).
pub const BOARDS_MAX_PAGE: u32 = 40;
/// The `class` filter's values (rivals and Showcases), from a row's `car_class_id` 0-6.
pub const BOARD_CLASSES: [&str; 7] = ["D", "C", "B", "A", "S1", "S2", "X"];

/// A board's score unit (§3): `m/s` and `s` are decimals.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoardUnit {
    /// Metres per second (speed traps and zones).
    #[serde(rename = "m/s")]
    MetresPerSecond,
    /// Seconds (race times).
    #[serde(rename = "s")]
    Seconds,
    /// A count (the ambient stats boards).
    #[serde(rename = "count")]
    Count,
    /// Points (best skill chain).
    #[serde(rename = "points")]
    Points,
    /// Experience (the global XP board).
    #[serde(rename = "xp")]
    Xp,
}

/// Which way a board ranks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoardBetter {
    /// Higher scores first.
    Higher,
    /// Lower scores first (times).
    Lower,
}

/// Where a speed camera stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoardRegion {
    /// The base map.
    #[serde(rename = "mainland")]
    Mainland,
    /// The Storm Island expansion.
    #[serde(rename = "storm-island")]
    StormIsland,
}

/// `BoardSummary.top`: the board's #1, only when that player is listed on the website.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardTop {
    /// The plain name.
    pub name: String,
    /// In the family's unit.
    pub score: f64,
    /// The opaque profile ref (§5.1): snapshot files only, never on a live route or a page.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// One board of a family (`GET /boards`, and `BoardPage.board`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardSummary {
    /// Camera id, `trackId`, event id or slug, as a string.
    pub board: String,
    /// The catalog's name (title case for cameras); the slug for stats and XP.
    pub title: String,
    /// Speed cameras: mainland or Storm Island.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<BoardRegion>,
    /// Rivals: the track's race modes ("StreetRace", ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disciplines: Option<Vec<String>>,
    /// Rivals: the championships using the track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub championships: Option<Vec<String>>,
    /// Tracks: length in metres.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_m: Option<i64>,
    /// Rows the board shows (game visibility, then website listing).
    pub entries: u64,
    /// The #1 when listed on the website.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<BoardTop>,
    /// `boards.json` of a snapshot (§5.1): the refs of the board's rows, in rank order, so the
    /// site recomputes `entries` and `top` over the live allowlist. Absent on live routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Vec<String>>,
}

/// One family of `GET /boards`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardFamily {
    /// The slug (`speed-traps`, `rivals`, ...).
    pub family: String,
    /// The family's unit (`stats` is `count`; its `skill-chain` board's page says `points`).
    pub unit: BoardUnit,
    /// Which way it ranks.
    pub better: BoardBetter,
    /// Every catalog board, empty ones included.
    pub boards: Vec<BoardSummary>,
}

/// `GET /boards?game&mode`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardIndex {
    /// In the order of DASHBOARD_API.md §3.
    pub families: Vec<BoardFamily>,
}

/// `BoardPlayer.club`: the player's car club of the board's mode (never a private club's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardClub {
    /// The club tag.
    pub tag: String,
}

/// A row's player.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardPlayer {
    /// The plain name (OD-120).
    pub name: String,
    /// The linked Discord account's global name, else its username (DB-Q2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
    /// The car club of the board's mode, unless private.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club: Option<BoardClub>,
    /// The opaque profile ref (§5.1): snapshot files only, never on a live route or a page.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// A row's car.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardCar {
    /// The game's car id.
    pub id: i32,
    /// The catalog's name ("Toyota 2000 GT 1969").
    pub name: String,
    /// The class letter of the row's `car_class_id` (`D` to `X`, `R`).
    pub class: String,
    /// The display PI (608).
    pub pi: i64,
}

/// A row's assists (the rivals screen's).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardAssists {
    /// ABS.
    pub abs: bool,
    /// Traction control.
    pub tcs: bool,
    /// Stability control.
    pub stm: bool,
    /// Auto brake.
    pub auto_brake: bool,
    /// Automatic gearbox.
    pub auto_shift: bool,
    /// Manual with clutch.
    pub clutch: bool,
}

/// One row of a board.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardRow {
    /// The game's rank (hidden rows leave gaps; never re-ranked).
    pub rank: u64,
    /// The player.
    pub player: BoardPlayer,
    /// In the board's unit.
    pub score: f64,
    /// The car; absent for stats and XP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car: Option<BoardCar>,
    /// Time boards: a clean run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean: Option<bool>,
    /// Time boards: the assists used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assists: Option<BoardAssists>,
    /// Submitted (RFC 3339 UTC).
    pub at: String,
}

/// `BoardPage.classes`: the rows of one class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardClassCount {
    /// `D` to `X`.
    pub class: String,
    /// Rows the board shows in it.
    pub entries: u64,
}

/// `GET /boards/{family}/{board}?game&mode&class&page`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardPage {
    /// The family slug.
    pub family: String,
    /// The board's unit.
    pub unit: BoardUnit,
    /// Which way it ranks.
    pub better: BoardBetter,
    /// The board (all classes).
    pub board: BoardSummary,
    /// Rivals and Showcases: per-class counts, `D` to `X`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classes: Option<Vec<BoardClassCount>>,
    /// Rows of the view (the class when filtered).
    pub total: u64,
    /// The page (1 to 40).
    pub page: u32,
    /// At most 25, in rank order.
    pub rows: Vec<BoardRow>,
}

/// Players per page of `GET /players` (DASHBOARD_API.md §4).
pub const PLAYERS_PAGE_SIZE: u32 = 25;
/// The last page of `GET /players`.
pub const PLAYERS_MAX_PAGE: u32 = 100;
/// The shortest `q` of `GET /players`.
pub const PLAYERS_QUERY_MIN: usize = 2;
/// The longest `q` of `GET /players`.
pub const PLAYERS_QUERY_MAX: usize = 15;
/// The largest avatar `GET /media/avatar/{name}` relays, in bytes.
pub const AVATAR_MAX_BYTES: usize = 524_288;
/// The rivals boards `PlayerProfile.bests` lists at most (the profile's best-ranked).
pub const BESTS_MAX_RIVALS: usize = 10;

/// How a stat row is shown (§4); the server sends the contract's units (`temperature` °C, `load`
/// N, both converted from the game's storage).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatFormat {
    /// An integer count.
    Int,
    /// Credits.
    Credits,
    /// Seconds.
    Duration,
    /// Metres.
    Distance,
    /// Metres per second.
    Speed,
    /// 0-100.
    Percent,
    /// A plain decimal average.
    Average,
    /// A car (`StatRow.car`).
    Car,
    /// A radio station (`StatRow.station` when named).
    Radio,
    /// g.
    Gforce,
    /// °C.
    Temperature,
    /// Newtons.
    Load,
    /// Metres.
    Stroke,
    /// `n / of`.
    Of,
}

impl StatFormat {
    /// The format of a layout row's name (`fi_gamedb::stats`).
    pub fn parse(text: &str) -> Option<StatFormat> {
        serde_json::from_value(serde_json::Value::String(text.to_owned())).ok()
    }
}

/// One row of a Statistics tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatRow {
    /// The layout's stable key (the site's label key).
    pub key: String,
    /// How it is shown.
    pub format: StatFormat,
    /// In the format's unit (a car id for `car`, a station id for `radio`).
    pub value: f64,
    /// `of`: the game's total.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<i64>,
    /// `car`: the car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car: Option<BoardCar>,
    /// `radio`: the station's name (absent for "None", which is the site's copy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station: Option<String>,
}

/// One Statistics tab with at least one row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatTab {
    /// `general`, `online`, `drivatar`, `discovery`, `single-player`, `skills`, `records`,
    /// `storm-island`.
    pub tab: String,
    /// In the game's display order.
    pub rows: Vec<StatRow>,
}

/// `PlayerCard.club`: the profile's car club of its mode (never a private club's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerClub {
    /// The tag.
    pub tag: String,
    /// The name.
    pub name: String,
}

/// A player's public card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerCard {
    /// The plain name (OD-120).
    pub name: String,
    /// The linked Discord account's global name, else its username (DB-Q2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The driver level the game shows, 1 to 999, from the XP total; absent without XP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    /// The XP total (the online profile's, else the Logon's); absent without XP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xp: Option<u64>,
    /// An active play session now.
    pub online: bool,
    /// The latest of the last logon, presence and session end (RFC 3339 UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// The car club of its mode, unless private.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub club: Option<PlayerClub>,
    /// The opaque profile ref (§5.1): snapshot files only, never on a live route or a page.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// `GET /players?game&mode&q&sort&page`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerList {
    /// Listed profiles matching.
    pub total: u64,
    /// The page (1 to 100).
    pub page: u32,
    /// At most 25.
    pub rows: Vec<PlayerCard>,
}

/// One board a profile has a row on, with its game rank.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersonalBest {
    /// §3 family slug.
    pub family: String,
    /// §3 board id.
    pub board: String,
    /// §3 board title.
    pub title: String,
    /// The board's unit.
    pub unit: BoardUnit,
    /// The game's rank (§3).
    pub rank: u64,
    /// In `unit`.
    pub score: f64,
    /// The car; absent for stats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car: Option<BoardCar>,
    /// Submitted (RFC 3339 UTC).
    pub at: String,
}

/// `GET /players/{name}?game`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerProfile {
    /// The card (its own mode).
    pub card: PlayerCard,
    /// Profile created (RFC 3339 UTC).
    pub since: String,
    /// Boards where it is the game's #1 (§3 ranking).
    pub records: u64,
    /// Speed traps, speed zones, Showcases, the 10 best-ranked rivals routes, stats boards.
    pub bests: Vec<PersonalBest>,
    /// The Statistics tabs with rows.
    pub tabs: Vec<StatTab>,
}

/// `fm5/profiles/<lower(name)>.json` (§5.3, CR-S109): one listed Forza Motorsport 5 career.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fm5Career {
    /// The card (its own stored mode; `level`/`xp` are the career's `profile_progress`, as the
    /// game sent them, never FH2's curve).
    pub card: PlayerCard,
    /// Career created (RFC 3339 UTC).
    pub since: String,
    /// The stored progress; every absent number is omitted.
    pub progress: Fm5Progress,
    /// The career's stored bests (`fm5_score`), one per key.
    pub bests: Vec<Fm5Best>,
}

/// An FM5 career's `profile_progress` numbers (GAME_PLATFORM.md §3a "Career and scores (M7)").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fm5Progress {
    /// `playerRaceHistory[0].level`, as the game sends it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    /// `playerRaceHistory[0].xp`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xp: Option<u64>,
    /// `playerHistory[0].credits`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<u64>,
    /// The stored `garageCars` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cars: Option<u32>,
}

/// One stored FM5 best (`Scoreboard.SubmitScore`'s key and score). No FM5 catalog exists yet, so
/// tracks and cars are ids only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fm5Best {
    /// `time` (seconds, lower is better) | `drift` (points, higher is better).
    pub kind: String,
    /// `scoreboardType`.
    pub scoreboard_type: i32,
    /// `gameScoreboardId` (-1 a private race).
    pub game_scoreboard_id: i32,
    /// `trackId`.
    pub track_id: i32,
    /// `carId`.
    pub car_id: i32,
    /// `carClassId`.
    pub car_class_id: i32,
    /// `carDriveTypeId`.
    pub car_drive_type_id: i32,
    /// The score as stored (f32).
    pub score: f64,
    /// `isClean`.
    pub clean: bool,
    /// When stored (RFC 3339 UTC).
    pub at: String,
}

/// The hourly snapshot's format version (`manifest.json` `version`, DASHBOARD_API.md §5).
pub const SNAPSHOT_VERSION: u32 = 1;
/// The snapshots kept (`current` and the two before it).
pub const SNAPSHOTS_KEPT: usize = 3;
/// The largest snapshot written, in bytes (512 MiB).
pub const SNAPSHOT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// `manifest.json` `games.<game>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotGame {
    /// The modes written (`hardcore`, `noob`).
    pub modes: Vec<String>,
}

/// `manifest.json` (§5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotManifest {
    /// [`SNAPSHOT_VERSION`].
    pub version: u32,
    /// When generated (RFC 3339 UTC).
    pub generated_at: String,
    /// `generatedAt` + the interval (an estimate: the scheduler runs one job at a time).
    pub next_at: String,
    /// The job's interval in seconds.
    pub interval_secs: u64,
    /// By game code.
    pub games: std::collections::BTreeMap<String, SnapshotGame>,
}

/// `<game>/<mode>/boards/<family>/<board>.json` (§5): every listed row of one board.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotBoard {
    /// The board (all classes).
    pub board: BoardSummary,
    /// Rivals and Showcases: per-class counts, `D` to `X`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classes: Option<Vec<BoardClassCount>>,
    /// Every listed row, game ranks kept.
    pub rows: Vec<BoardRow>,
}

/// `<game>/<mode>/players.json` (§5): every listed profile of the mode.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPlayers {
    /// By name.
    pub rows: Vec<PlayerCard>,
}

/// The length of a profile ref (§5.1): the first 22 characters of the HMAC's base64url.
pub const REF_LENGTH: usize = 22;

/// One profile of `GET /listed` (§5.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedProfile {
    /// The opaque ref (server-side only).
    #[serde(rename = "ref")]
    pub ref_: String,
    /// The current lower-case plain name.
    pub name: String,
}

/// One club of `GET /listed` (§5.2): not private now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedClub {
    /// `hardcore` | `noob`.
    pub mode: String,
    /// The tag (capitals).
    pub tag: String,
}

/// `GET /listed?game` (§5.1, §5.2): every profile of the game listed on the website now, and
/// every club of the game that is not private now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedProfiles {
    /// By name.
    pub profiles: Vec<ListedProfile>,
    /// By mode, then tag.
    pub clubs: Vec<ListedClub>,
    /// The opaque ids of every shared creation shown now (§7 "As built").
    pub creations: Vec<String>,
}

/// A person shown on a club (§6): its leader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubPerson {
    /// The plain name.
    pub name: String,
    /// The linked Discord account's global name, else its username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
    /// The opaque ref (§5.1): snapshot files only.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// A club as the lists show it (§6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubCard {
    /// The tag (capitals).
    pub tag: String,
    /// The name.
    pub name: String,
    /// Visible members (the game's count).
    pub members: u64,
    /// The members' XP earned in the club (the game's club XP).
    pub xp: f64,
    /// The game's club XP rank in the mode (ties share it).
    pub rank: u64,
    /// The game's club level, when derivable from stored data (it is not today: absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    /// Region keyword id (`south-america`, ...), else the raw id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Attitude keyword id (`casual`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attitude: Option<String>,
    /// Interest keyword id (`racing`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interest: Option<String>,
    /// Created (RFC 3339 UTC).
    pub created_at: String,
    /// The leader, when visible and listed on the website.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader: Option<ClubPerson>,
}

/// One member of a club page's roster (§6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubMember {
    /// The plain name.
    pub name: String,
    /// The linked Discord account's global name, else its username.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_name: Option<String>,
    /// A Discord avatar exists.
    pub avatar: bool,
    /// The opaque ref (§5.1): snapshot files only.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
    /// `leader` | `officer` | `member`.
    pub role: String,
    /// XP earned in the club.
    pub xp: f64,
    /// Joined (RFC 3339 UTC).
    pub joined_at: String,
    /// The driver level the game shows (§4), when its XP is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
}

/// `ClubPage.champion`: the game's ladder champion (§6).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubChampion {
    /// The plain name.
    pub name: String,
    /// The driver level the game shows (§4).
    pub level: u32,
    /// The opaque ref (§5.1): snapshot files only.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// One entry of `ClubPage.weekly` (§6; the game's stored data has no weekly ladder today, so the
/// field is absent).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubWeekly {
    /// The plain name.
    pub name: String,
    /// XP this week.
    pub xp: f64,
    /// The opaque ref (§5.1): snapshot files only.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// One record of a club page: a board whose game #1 is a member (§6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubRecord {
    /// §3 family slug.
    pub family: String,
    /// §3 board id.
    pub board: String,
    /// §3 board title.
    pub title: String,
    /// The holder's plain name.
    pub holder: String,
    /// In `unit`.
    pub score: f64,
    /// The board's unit.
    pub unit: BoardUnit,
    /// The holder's opaque ref (§5.1): snapshot files only.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// `GET /clubs/{tag}?game&mode`, and `<game>/<mode>/clubs/<TAG>.json` (§6).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubPage {
    /// The card.
    pub card: ClubCard,
    /// The description, when not empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The ladder champion, when visible, listed and with a known level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub champion: Option<ClubChampion>,
    /// `open` | `request` | `invite`.
    pub privacy: String,
    /// The listed members, by role then XP.
    pub members: Vec<ClubMember>,
    /// The weekly XP ladder, when the stored data has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<Vec<ClubWeekly>>,
    /// Boards whose game #1 is a member.
    pub records: Vec<ClubRecord>,
}

/// `GET /clubs?game&mode`, and `<game>/<mode>/clubs.json` (§6): by XP rank.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClubList {
    /// Every non-private club of the mode.
    pub rows: Vec<ClubCard>,
}

/// One car of a garage (§7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GarageCar {
    /// The car (catalog name, stock class and display PI).
    pub car: BoardCar,
    /// Races in it (`numRaces`).
    pub races: u64,
}

/// A creation's author (§7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreationAuthor {
    /// The plain name.
    pub name: String,
    /// The opaque ref (§5.1): snapshot files only.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// A creation shared to the game's storefront (§7).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Creation {
    /// Opaque: base64url(HMAC(creation_key, "fi-dashboard-creation-v2:" || storefront id)), 22
    /// characters, `creation_key` a sub-key of FI_DASHBOARD_KEY (§5.1 "As built"). Never the
    /// storefront id.
    pub id: String,
    /// `livery` | `group` | `tune`.
    pub kind: String,
    /// The title.
    pub title: String,
    /// The description, when not empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The creation's car, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub car: Option<BoardCar>,
    /// The author.
    pub author: CreationAuthor,
    /// Downloads.
    pub downloads: u64,
    /// Likes.
    pub likes: u64,
    /// Uses.
    pub uses: u64,
    /// Shared (RFC 3339 UTC).
    pub created_at: String,
    /// A preview image is served (`GET /media/creation/{id}?file`).
    pub preview: bool,
    /// The storefront id: **server-only**, in snapshot files for the preview lookup (sent back as
    /// `?file=`), stripped by the site like refs and never sent to a browser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<i32>,
}

/// `fh2/profiles/<lower(name)>.garage.json` (§7).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerGarage {
    /// By class, then PI, then name.
    pub cars: Vec<GarageCar>,
    /// The player's own shared creations, newest first.
    pub creations: Vec<Creation>,
}

/// `fh2/<mode>/creations.json` (§7): every shared creation of the mode, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotCreations {
    /// Newest first.
    pub rows: Vec<Creation>,
}

/// A catalog car's pack (§7.1): absent for a base-game car.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarPack {
    /// The pack's `mod.json` id ("fm5-zs-retro-supercars-pack").
    pub id: String,
    /// The game the pack's cars come from ("FM5", "FM6").
    pub game: String,
}

/// One listed profile of the mode that has the car in its garage (§7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CarOwner {
    /// The plain name.
    pub name: String,
    /// Races in the car (`numRaces`).
    pub races: u64,
    /// The profile's opaque ref (§5.1), snapshot files only.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub ref_: Option<String>,
}

/// One catalog car, its stock stats and its owners in the mode (§7.1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogCar {
    /// The car (catalog name, stock class and display PI).
    pub car: BoardCar,
    /// The make's display name ("Aston Martin").
    pub make: String,
    /// Model year.
    pub year: i64,
    /// Stock top speed, km/h (rounded).
    pub top_speed_kph: u32,
    /// `Data_Car.BaseCost`, credits, when the catalog has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_cost: Option<i64>,
    /// The car pack, for a pack car.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack: Option<CarPack>,
    /// Listed owners of the mode, most races first, then name.
    pub owners: Vec<CarOwner>,
}

/// `fh2/<mode>/cars.json` (§7.1): every catalog car (base game, then packs, each by class, PI,
/// name) with the mode's listed owners.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotCars {
    /// Base-game cars first, then pack cars.
    pub cars: Vec<CatalogCar>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_shapes_are_camel_case_and_skip_absent_keys() {
        let live = Live {
            at: "2026-11-01T12:00:00.000000Z".into(),
            game: "fh2".into(),
            online: 3,
            per_mode: ModeCounts {
                hardcore: 2,
                noob: 1,
            },
            lobbies: LiveLobbies {
                open: 2,
                per_mode: ModeCounts {
                    hardcore: 2,
                    noob: 1,
                },
            },
            capacity: None,
        };
        assert_eq!(
            serde_json::to_value(&live).unwrap(),
            json!({"at": "2026-11-01T12:00:00.000000Z", "game": "fh2", "online": 3,
                "perMode": {"hardcore": 2, "noob": 1},
                "lobbies": {"open": 2, "perMode": {"hardcore": 2, "noob": 1}}, "capacity": null})
        );
        let lobby = Lobby {
            world_id: "0192f0c4-0000-7000-8000-000000000001".into(),
            name: "Cruise".into(),
            host: Some(LobbyHost {
                name: "Alex".into(),
                discord_name: None,
                avatar: false,
            }),
            players: 1,
            max_players: 8,
            full: false,
            activity: "freeroam".into(),
            region: None,
            mods: false,
            dedicated: false,
            any_mode: true,
            created_at: "2026-11-01T12:00:00.000000Z".into(),
        };
        assert_eq!(
            serde_json::to_value(&lobby).unwrap(),
            json!({"worldId": "0192f0c4-0000-7000-8000-000000000001", "name": "Cruise",
                "host": {"name": "Alex", "avatar": false}, "players": 1, "maxPlayers": 8,
                "full": false, "activity": "freeroam", "mods": false, "dedicated": false,
                "anyMode": true, "createdAt": "2026-11-01T12:00:00.000000Z"})
        );
        let hidden = Lobby {
            host: None,
            ..lobby
        };
        assert!(serde_json::to_value(&hidden).unwrap().get("host").is_none());
    }

    #[test]
    fn board_shapes_use_the_contract_spellings() {
        assert_eq!(
            serde_json::to_value([
                BoardUnit::MetresPerSecond,
                BoardUnit::Seconds,
                BoardUnit::Count,
                BoardUnit::Points,
                BoardUnit::Xp
            ])
            .unwrap(),
            json!(["m/s", "s", "count", "points", "xp"])
        );
        assert_eq!(
            serde_json::to_value((BoardBetter::Higher, BoardRegion::StormIsland)).unwrap(),
            json!(["higher", "storm-island"])
        );
        let row = BoardRow {
            rank: 3,
            player: BoardPlayer {
                name: "Driver".into(),
                discord_name: None,
                avatar: false,
                club: Some(BoardClub { tag: "AB".into() }),
                ref_: None,
            },
            score: 93.25,
            car: None,
            clean: None,
            assists: None,
            at: "2026-11-01T12:00:00.000000Z".into(),
        };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            json!({"rank": 3, "player": {"name": "Driver", "avatar": false, "club": {"tag": "AB"}},
                "score": 93.25, "at": "2026-11-01T12:00:00.000000Z"})
        );
        let summary = BoardSummary {
            board: "1".into(),
            title: "South San Giovanni Speed Trap".into(),
            region: Some(BoardRegion::Mainland),
            disciplines: None,
            championships: None,
            length_m: None,
            entries: 0,
            top: None,
            refs: None,
        };
        assert_eq!(
            serde_json::to_value(&summary).unwrap(),
            json!({"board": "1", "title": "South San Giovanni Speed Trap", "region": "mainland",
                "entries": 0})
        );
    }
}
/// Private offline Explore processor input (EXPLORE.md, CR-S94); never a browser DTO.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotGhostRecording {
    /// Live visibility join key, stripped before rendering.
    pub r#ref: String,
    /// Current public profile name.
    pub name: String,
    /// Rivals or Showcases; a shared track must never mix their recordings.
    pub family: String,
    /// Dashboard board key (track id for Rivals, event id for Showcases).
    pub board: String,
    /// Native rivals track id.
    pub track_id: i32,
    /// Exact score owning the ghost, in seconds.
    pub score_seconds: f64,
    /// Verified local blob object name, not a public URL.
    pub sha256: String,
    /// Compressed blob size, at most MAX_GHOST.
    pub size: i32,
}

/// Every listed current personal-best route recording of one game/mode.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotGhostSource {
    /// Listed, current personal bests with their own recording.
    pub recordings: Vec<SnapshotGhostRecording>,
}

/// One private derived replay candidate (EXPLORE.md); refs remain server-side until SSR selection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplayRecording {
    /// Live visibility join key.
    pub r#ref: String,
    /// Listed name at the source generation.
    pub display_name: String,
    /// Mode-correct career level, null if unavailable.
    pub level: Option<u32>,
    /// Whether the standard avatar proxy can supply an image.
    pub avatar: bool,
    /// Exact source score's car.
    pub car: BoardCar,
    /// Game rank, without renumbering hidden profiles.
    pub rank: u64,
    /// Score owning the recording, seconds.
    pub score_seconds: f64,
    /// Last decoded sample time, seconds.
    pub duration_seconds: f64,
    /// Timestamp, native X and native Z, in seconds/metres.
    pub points: Vec<[f64; 3]>,
    /// Native Y in metres, aligned to points.
    pub elevation_m: Vec<f64>,
    /// Derived 3D arc/time speed, aligned to points.
    pub speed_kmh: Vec<f64>,
}

/// Private event/class candidates, filtered by SSR before its20-driver limit.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SnapshotReplayFile {
    /// Decoded candidates only, never raw game recording containers.
    pub recordings: Vec<SnapshotReplayRecording>,
}

/// Lightweight private availability index; profile pages never read trajectory shards.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplayIndex {
    /// Decoded, mapped candidates before current visibility/score filtering and the20-driver cap.
    pub records: Vec<SnapshotReplayCandidate>,
    /// Pins currently supported by this game's map, including cameras without replay.
    pub mapped_events: Vec<ExploreBoardRef>,
}

/// Private availability metadata for one decoded score; no positions or raw recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotReplayCandidate {
    /// Current visibility join key, never rendered.
    pub r#ref: String,
    /// Snapshot public name.
    pub display_name: String,
    /// Rivals or Showcases.
    pub family: String,
    /// Canonical dashboard board key.
    pub board: String,
    /// Exact score owning this decoded ghost.
    pub score_seconds: f64,
    /// Car class used before the20-driver selection.
    pub r#class: String,
}

/// Private map catalog event; geometry and timing are versioned native asset-adapter output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreEvent {
    /// Stable map event key.
    pub id: String,
    /// Localized/catalog display name.
    pub name: String,
    /// Game event family.
    pub r#type: String,
    /// Region key.
    pub region: String,
    /// Route id used by the rivals board.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<i32>,
    /// Existing dashboard board reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<ExploreBoardRef>,
    /// Native asset adapter's selected-event geometry (not in the catalog index).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub geometry: Option<serde_json::Value>,
    /// Other versioned adapter metadata (anchor, disciplines, timing, grid, lengthM).
    #[serde(flatten)]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

/// An existing leaderboard addressed from the world map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExploreBoardRef {
    /// Canonical dashboard family slug.
    pub family: String,
    /// Canonical board key.
    pub id: String,
}

/// Game-independent map image metadata; native projection fields belong to the asset adapter.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExploreRegion {
    /// Region key.
    pub id: String,
    /// Local map texture asset path.
    pub image: String,
    /// Image dimensions and validated/native projection metadata.
    #[serde(flatten)]
    pub metadata: std::collections::BTreeMap<String, serde_json::Value>,
}

/// Private map index consumed by SSR; includes no unselected event geometry or replay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreCatalog {
    /// Asset-adapter schema revision.
    pub schema_version: u32,
    /// Registered game code.
    pub game: String,
    /// Game title.
    pub title: String,
    /// Map image/projection metadata.
    pub regions: Vec<ExploreRegion>,
    /// Compact pin/event metadata.
    pub events: Vec<ExploreEvent>,
    /// Versioned native road-overlay asset data.
    pub roads: serde_json::Value,
    /// Car labels from the game's catalog.
    pub car_names: std::collections::BTreeMap<String, String>,
}

/// Localized names from installed game stringtables (CR-S95).
pub type AssetNames = std::collections::BTreeMap<String, String>;

/// Private offline game asset catalog shape (GameAssetCar, CR-S95).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameAssetCar {
    /// Native asset field: id.
    pub id: i32,
    /// Native asset field: year.
    pub year: i32,
    /// Native asset field: make.
    pub make: AssetNames,
    /// Native asset field: model.
    pub model: AssetNames,
    /// Native asset field: media_name.
    pub media_name: String,
    /// Native asset field: class.
    pub r#class: String,
    /// Native asset field: thumbnail.
    pub thumbnail: bool,
    /// Native asset field: installed.
    pub installed: bool,
    /// Native asset field: drivable.
    pub drivable: bool,
}

/// Private offline game asset catalog shape (GameAssetCars, CR-S95).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameAssetCars {
    /// Native asset field: cars.
    pub cars: Vec<GameAssetCar>,
}

/// Private offline game asset catalog shape (GameAssetRace, CR-S95).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameAssetRace {
    /// Native asset field: id.
    pub id: i32,
    /// Native asset field: race_id.
    pub race_id: i32,
    /// Native asset field: track_id.
    pub track_id: i32,
    /// Native asset field: order.
    pub order: i32,
    /// Native asset field: series_id.
    pub series_id: i32,
    /// Native asset field: name.
    pub name: AssetNames,
    /// Native asset field: laps.
    pub laps: i32,
    /// Native asset field: drivers.
    pub drivers: i32,
    /// Native asset field: target_class.
    pub target_class: Option<String>,
    /// Uninterpreted native event class value; not the car-class table id.
    pub target_class_id: i32,
    /// Series target performance index, directly from MiniCareers.
    pub target_pi: i32,
    /// Localized native career-event type.
    pub kind: AssetNames,
    /// Original series artwork.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    /// Original native event-type artwork.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_poster: Option<String>,
    /// Native asset field: career_type_id.
    pub career_type_id: i32,
    /// Native asset field: bonus.
    pub bonus: bool,
}

/// Private offline game asset catalog shape (GameAssetTrack, CR-S95).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameAssetTrack {
    /// Native asset field: id.
    pub id: i32,
    /// Native asset field: venue_id.
    pub venue_id: i32,
    /// Native asset field: venue.
    pub venue: AssetNames,
    /// Native asset field: name.
    pub name: AssetNames,
    /// Native asset field: media_name.
    pub media_name: String,
    /// Native asset field: ribbon_index.
    pub ribbon_index: i32,
    /// Native asset field: reverse_index.
    pub reverse_index: i32,
    /// Native asset field: reverse.
    pub reverse: bool,
    /// Native asset field: active.
    pub active: bool,
    /// Native asset field: homespace.
    pub homespace: bool,
    /// Native asset field: length_m.
    pub length_m: f64,
    /// Native asset field: real_world.
    pub real_world: bool,
    /// Native asset field: start_line_fraction.
    pub start_line_fraction: f64,
    /// Native asset field: finish_line_fraction.
    pub finish_line_fraction: f64,
    /// Native asset field: events.
    pub events: Vec<GameAssetRace>,
    /// Native asset field: geometry_available.
    pub geometry_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Native asset field: map.
    pub map: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Native asset field: outline.
    pub outline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Native asset field: logo.
    pub logo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Native asset field: photo.
    pub photo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Native asset field: geometry.
    pub geometry: Option<serde_json::Value>,
}

/// Private offline game asset catalog shape (GameAssetCatalog, CR-S95).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GameAssetCatalog {
    /// Native asset field: schema_version.
    pub schema_version: u32,
    /// Native asset field: game.
    pub game: String,
    /// Native asset field: title.
    pub title: String,
    /// Native asset field: tracks.
    pub tracks: Vec<GameAssetTrack>,
    /// Native asset field: coverage.
    pub coverage: std::collections::BTreeMap<String, serde_json::Value>,
    /// Native asset field: source.
    pub source: std::collections::BTreeMap<String, serde_json::Value>,
}

/// FH3 Explore asset: an axis-aligned native map frame (CR-S107, FH3_EXPLORE.md).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetBounds {
    /// Native world X at the map's left edge.
    pub min_x: f64,
    /// Native world X at the map's right edge.
    pub max_x: f64,
    /// Native world Z at the map's top edge.
    pub min_z: f64,
    /// Native world Z at the map's bottom edge.
    pub max_z: f64,
}

/// FH3 Explore asset: one world map (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetRegion {
    /// Region id (`australia`, `mountain`, `island`).
    pub id: String,
    /// Localized region name.
    pub name: AssetNames,
    /// Map image path under the asset base.
    pub image: String,
    /// Logical map width in projection pixels (the file may be smaller).
    pub image_width: u32,
    /// Logical map height in projection pixels.
    pub image_height: u32,
    /// Native world bounds of the map image.
    pub bounds: Fh3AssetBounds,
}

/// FH3 Explore asset: one career race definition of a route (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetRace {
    /// Native race id.
    pub id: i32,
    /// Localized race name.
    pub name: AssetNames,
    /// Localized native race kind.
    pub kind: AssetNames,
    /// Native lap count.
    pub laps: i32,
    /// Native AI driver count.
    pub ai_drivers: i32,
}

/// FH3 Explore asset: one map event or place (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetEvent {
    /// `races:<trackId>` or `<type>:<region>:<entity>`.
    pub id: String,
    /// Event family (`races`, `speed-traps`, `bonus-boards`, ...).
    pub r#type: String,
    /// Region id.
    pub region: String,
    /// Localized name.
    pub name: AssetNames,
    /// Whether `events/<id>.json` holds native geometry.
    pub geometry_available: bool,
    /// Localized description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<AssetNames>,
    /// Native map position `[x, z]`; absent when unresolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anchor: Option<[f64; 2]>,
    /// Course length in metres.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length_m: Option<f64>,
    /// Native medal thresholds (`BronzeScore`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub medals: Option<std::collections::BTreeMap<String, f64>>,
    /// Career race definitions on this route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub races: Option<Vec<Fh3AssetRace>>,
    /// Native race disciplines of a route (`Scramble`, `CrossCountry`, ...); picks the map icon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disciplines: Option<Vec<String>>,
}

/// FH3 Explore asset: an event without a validated native position or geometry (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetUnavailable {
    /// Event id.
    pub id: String,
    /// Why it is not placed.
    pub reason: String,
}

/// FH3 Explore asset: `catalog.json` (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetCatalog {
    /// Catalog schema version (1).
    pub schema_version: u32,
    /// Always `fh3`.
    pub game: String,
    /// Display title.
    pub title: String,
    /// World maps.
    pub regions: Vec<Fh3AssetRegion>,
    /// Map events and places.
    pub events: Vec<Fh3AssetEvent>,
    /// Recovery counts.
    pub coverage: std::collections::BTreeMap<String, serde_json::Value>,
    /// Capability flags; `records` and `ghosts` are false.
    pub capabilities: std::collections::BTreeMap<String, bool>,
    /// Events not placed.
    pub unavailable: Vec<Fh3AssetUnavailable>,
}

/// FH3 Explore asset: one car (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetCar {
    /// Native car id.
    pub id: i32,
    /// Model year.
    pub year: i32,
    /// Localized full name.
    pub name: AssetNames,
    /// Localized make.
    pub make: AssetNames,
    /// Localized model.
    pub model: AssetNames,
    /// Localized class letter.
    pub r#class: AssetNames,
    /// Thumbnail path under the car image store.
    pub thumbnail: String,
    /// Native drivable flag.
    pub drivable: bool,
    /// Native installed flag.
    pub installed: bool,
}

/// FH3 Explore asset: `cars.json` (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetCars {
    /// Always `fh3`.
    pub game: String,
    /// Cars.
    pub cars: Vec<Fh3AssetCar>,
}

/// FH3 Explore asset: `events/<id>.json`, one event and its native geometry (CR-S107).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fh3AssetEventDetail {
    /// The event.
    pub event: Fh3AssetEvent,
    /// Native geometry, passed to the viewer unchanged.
    pub geometry: serde_json::Value,
}
