//! The hourly snapshot's content (DASHBOARD_API.md §5 and §5.1, OD-173): what `fi-cli
//! dashboard-snapshot` writes, built from the same reads and the same assembly as the live routes
//! ([`super::boards`], [`super::players`]), so a snapshot file is what `/boards*` and `/players*`
//! answer at that moment, plus each person's opaque `ref` ([`super::RefKey`], §5.1). This module
//! builds and serializes; where the bytes go (the files, the atomic swap) is the [`Sink`]'s.
//!
//! **Batched.** Per mode: one grouped ranking statement per board family
//! (`fi_db::dashboard::boards::family_rows_in`: every listed row of every board of the family),
//! one statement for the mode's players, their stored online profiles in chunks, and two for the
//! clubs (the game's ClubData statement and the grouped roster; §6), one for the shared creations
//! and one garage statement per chunk of players (§7). The index,
//! each board's counts and `top`, and every profile's `records` and `bests` are derived in memory
//! from those rows (a listed profile's rows are all present). Nothing is queried per board or per
//! profile. The caller runs every read in one `REPEATABLE READ READ ONLY` transaction.
//!
//! **Bounded.** Each file is serialized and handed to the sink as soon as it is complete; a
//! family's rows are dropped once its board files are out, keeping only each profile's compact
//! ranks. The bytes handed out plus an estimate of what is still held are checked against the
//! size guard after every file, so an oversized snapshot stops early instead of after everything
//! is in memory.

use super::RefKey;
use super::boards::{self, FAMILIES, Fam, class_id, family_read, in_unit};
use super::clubs::{BoardTopRow, club_card, club_page};
use super::garages::{catalog_cars, catalog_file, creation, garage_cars};
use super::players::{RankedFamily, card, records_and_bests, stat_tabs};
use crate::views::at;
use fi_db::clubs as db_clubs;
use fi_db::dashboard::boards::{self as db_boards, ProfileRank, WebRow};
use fi_db::dashboard::players as db_players;
use fi_db::identity::IdentityError;
use fi_protocol::dashboard::{
    BOARD_CLASSES, BoardClassCount, BoardFamily, BoardIndex, BoardTop, CarOwner, ClubList,
    Creation, Fm5Best, Fm5Career, Fm5Progress, PlayerGarage, PlayerProfile, SnapshotBoard,
    SnapshotCreations, SnapshotPlayers,
};
use sqlx::PgConnection;
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Where the snapshot's files go.
pub trait Sink {
    /// Takes the complete file `rel` (a relative path under the snapshot).
    fn put(&mut self, rel: &str, bytes: Vec<u8>) -> Result<(), String>;
    /// The bytes taken so far.
    fn bytes(&self) -> u64;
}

/// Why a snapshot stopped.
#[derive(Debug)]
pub enum SnapshotError {
    /// A read failed.
    Db(IdentityError),
    /// A file could not be serialized.
    Json(serde_json::Error),
    /// The sink refused a file.
    Write(String),
    /// The size guard (bytes written plus what is still held).
    TooLarge(u64),
    /// Two listed profiles share one lower-case name (impossible in one consistent read: names
    /// are unique per game); refused rather than overwrite one profile's file with another's.
    DuplicateProfile,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Db(e) => write!(f, "database: {e}"),
            SnapshotError::Json(e) => write!(f, "serializing: {e}"),
            SnapshotError::Write(e) => write!(f, "writing: {e}"),
            SnapshotError::TooLarge(max) => write!(
                f,
                "the snapshot passed its size guard ({max} bytes); nothing was swapped in"
            ),
            SnapshotError::DuplicateProfile => f.write_str(
                "two listed profiles share one lower-case name; the snapshot was refused",
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<IdentityError> for SnapshotError {
    fn from(e: IdentityError) -> Self {
        SnapshotError::Db(e)
    }
}

impl From<serde_json::Error> for SnapshotError {
    fn from(e: serde_json::Error) -> Self {
        SnapshotError::Json(e)
    }
}

/// What a snapshot holds (logged as counts, never names).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Board files.
    pub boards: u64,
    /// Board rows in all.
    pub rows: u64,
    /// Player cards in all.
    pub players: u64,
    /// Profile files.
    pub profiles: u64,
    /// Club pages.
    pub clubs: u64,
    /// Garage files.
    pub garages: u64,
    /// Shared creations.
    pub creations: u64,
    /// FM5 career files (§5.3).
    pub careers: u64,
}

/// An estimate of the memory one held profile rank costs (the rank, a catalog board's clone).
const HELD_PER_RANK: u64 = 512;
/// Stored online profiles read per statement.
const DATA_CHUNK: usize = 500;

/// Builds one snapshot, mode after mode, into a [`Sink`].
pub struct Snapshotter<'a> {
    refs: &'a RefKey,
    max_bytes: u64,
    names: HashSet<String>,
    counts: Counts,
}

impl<'a> Snapshotter<'a> {
    /// A snapshot keyed by `refs`, refused past `max_bytes`.
    pub fn new(refs: &'a RefKey, max_bytes: u64) -> Self {
        Snapshotter {
            refs,
            max_bytes,
            names: HashSet::new(),
            counts: Counts::default(),
        }
    }

    /// What was written so far.
    pub fn counts(&self) -> Counts {
        self.counts
    }

    fn put<T: serde::Serialize>(
        &self,
        sink: &mut dyn Sink,
        rel: &str,
        value: &T,
        held: u64,
    ) -> Result<(), SnapshotError> {
        let bytes = serde_json::to_vec(value)?;
        if sink.bytes() + bytes.len() as u64 + held > self.max_bytes {
            return Err(SnapshotError::TooLarge(self.max_bytes));
        }
        sink.put(rel, bytes).map_err(SnapshotError::Write)
    }

    /// One board's file from its listed rows (in rank order), and its `boards.json` summary
    /// (with the rows' refs).
    fn board_file(
        &self,
        fam: Fam,
        board: &boards::Board,
        rows: &[WebRow],
    ) -> (SnapshotBoard, fi_protocol::dashboard::BoardSummary) {
        let mut summary = board.summary.clone();
        summary.entries = rows.len() as u64;
        // The game's #1, only when listed (never the next listed row).
        summary.top = rows.first().filter(|r| r.rank == 1).map(|r| BoardTop {
            name: r.name.clone(),
            score: in_unit(board.unit, r.score),
            ref_: Some(self.refs.of(r.profile_id)),
        });
        let classes = fam.classed().then(|| {
            BOARD_CLASSES
                .iter()
                .map(|c| BoardClassCount {
                    class: (*c).to_owned(),
                    entries: class_id(c)
                        .map_or(0, |id| rows.iter().filter(|r| r.car_class_id == id).count())
                        as u64,
                })
                .collect()
        });
        let file = SnapshotBoard {
            board: summary.clone(),
            classes,
            rows: rows
                .iter()
                .map(|r| {
                    let mut row = boards::row(fam, board.unit, r.clone());
                    row.player.ref_ = Some(self.refs.of(r.profile_id));
                    row
                })
                .collect(),
        };
        summary.refs = Some(rows.iter().map(|r| self.refs.of(r.profile_id)).collect());
        (file, summary)
    }

    /// One mode of `game` (Monthly Rivals up to `month`): `<game>/<mode>/boards/...`,
    /// `<game>/<mode>/boards.json`, `<game>/<mode>/players.json` and `<game>/profiles/<name>.json`
    /// of the mode's listed profiles.
    pub async fn mode_in(
        &mut self,
        conn: &mut PgConnection,
        game: &str,
        mode: &str,
        month: i64,
        sink: &mut dyn Sink,
    ) -> Result<(), SnapshotError> {
        let base = format!("{game}/{mode}");
        // CR-S94: private references only. The offline processor reads the immutable blobs
        // after the transaction; no filesystem/network I/O or raw ghost is held here.
        let mut ghosts = fi_protocol::dashboard::SnapshotGhostSource::default();
        for family in [Fam::Rivals, Fam::Showcases] {
            let catalog = boards::family_boards(family, month);
            let tracks: Vec<i32> = catalog.iter().map(|b| b.key).collect();
            let sources =
                db_boards::replay_sources_in(conn, game, mode, family.key().0, &tracks).await?;
            for r in sources {
                let Some(board) = catalog.iter().find(|b| b.key == r.track_id) else {
                    continue;
                };
                ghosts
                    .recordings
                    .push(fi_protocol::dashboard::SnapshotGhostRecording {
                        r#ref: self.refs.of(r.profile_id),
                        name: r.name,
                        family: family.slug().to_owned(),
                        board: board.summary.board.clone(),
                        track_id: r.track_id,
                        score_seconds: r.score,
                        sha256: r.sha256,
                        size: r.size,
                    });
            }
        }
        self.put(sink, &format!("{base}/ghosts.json"), &ghosts, 0)?;
        let mut index = BoardIndex::default();
        // Per profile: (family, its boards in catalog order, its ranks on them).
        let mut ranks: HashMap<Uuid, Vec<RankedFamily>> = HashMap::new();
        let mut held: u64 = 0;
        // Each board's listed #1 (a club page's records).
        let mut tops: Vec<BoardTopRow> = Vec::new();
        for fam in FAMILIES {
            let catalog = boards::family_boards(fam, month);
            let keys: Vec<i32> = catalog.iter().map(|b| b.key).collect();
            let rows =
                db_boards::family_rows_in(conn, game, mode, &family_read(fam, &keys)).await?;
            let mut by_board: HashMap<i32, Vec<WebRow>> = HashMap::new();
            for (board, row) in rows {
                by_board.entry(board).or_default().push(row);
            }
            let mut summaries = Vec::with_capacity(catalog.len());
            for b in &catalog {
                let rows = by_board.remove(&b.key).unwrap_or_default();
                let (file, summary) = self.board_file(fam, b, &rows);
                self.put(
                    sink,
                    &format!("{base}/boards/{}/{}.json", fam.slug(), b.summary.board),
                    &file,
                    held,
                )?;
                self.counts.boards += 1;
                self.counts.rows += rows.len() as u64;
                if let Some(first) = rows.first().filter(|r| r.rank == 1) {
                    tops.push(BoardTopRow {
                        fam,
                        board: b.clone(),
                        profile_id: first.profile_id,
                        name: first.name.clone(),
                        score: first.score,
                    });
                    held += HELD_PER_RANK;
                }
                summaries.push(summary);
                for r in &rows {
                    let rank = ProfileRank {
                        board: b.key,
                        rank: r.rank,
                        score: r.score,
                        submitted_at: r.submitted_at,
                        car_id: r.car_id,
                        car_class_id: r.car_class_id,
                        car_pi: r.car_pi,
                    };
                    let entry = ranks.entry(r.profile_id).or_default();
                    match entry.last_mut() {
                        Some((f, boards, rs)) if *f == fam => {
                            boards.push(b.clone());
                            rs.push(rank);
                        }
                        _ => entry.push((fam, vec![b.clone()], vec![rank])),
                    }
                    held += HELD_PER_RANK;
                }
            }
            index.families.push(BoardFamily {
                family: fam.slug().to_owned(),
                unit: fam.unit(),
                better: fam.better(),
                boards: summaries,
            });
        }
        self.put(sink, &format!("{base}/boards.json"), &index, held)?;
        drop(index);
        // Shared creations of the mode (§7): one statement; every file's owner is listed.
        let files = fi_db::storefront::shared_files_in(conn, game, Some(mode)).await?;
        let mut own: HashMap<Uuid, Vec<Creation>> = HashMap::new();
        let mut all = Vec::with_capacity(files.len());
        for f in &files {
            own.entry(f.profile_id)
                .or_default()
                .push(creation(f, self.refs, true));
            all.push(creation(f, self.refs, true));
        }
        drop(files);
        self.counts.creations += all.len() as u64;
        self.put(
            sink,
            &format!("{base}/creations.json"),
            &SnapshotCreations { rows: all },
            held,
        )?;
        let players = db_players::every_player_in(conn, game, mode).await?;
        let mut cards = Vec::with_capacity(players.len());
        // §7.1: the mode's listed owners of each car, from the same garage rows.
        let mut owners: HashMap<i32, Vec<CarOwner>> = HashMap::new();
        for chunk in players.chunks(DATA_CHUNK) {
            let ids: Vec<Uuid> = chunk.iter().map(|p| p.profile_id).collect();
            let data: HashMap<Uuid, serde_json::Value> =
                db_players::profile_data_in(conn, game, &ids)
                    .await?
                    .into_iter()
                    .collect();
            let mut garages: HashMap<Uuid, Vec<(i64, i64)>> = HashMap::new();
            for (profile, car, races) in
                db_players::garages_in(conn, game, &ids, catalog_cars()).await?
            {
                garages.entry(profile).or_default().push((car, races));
            }
            for p in chunk {
                let name = p.name.to_lowercase();
                // Per game: a gamertag is global, so one name has a career in every game.
                if !self.names.insert(format!("{game}/{name}")) {
                    return Err(SnapshotError::DuplicateProfile);
                }
                let mut c = card(p);
                c.ref_ = Some(self.refs.of(p.profile_id));
                let (records, bests) =
                    records_and_bests(ranks.remove(&p.profile_id).as_deref().unwrap_or_default());
                let profile = PlayerProfile {
                    card: c.clone(),
                    since: at(p.since),
                    records,
                    bests,
                    tabs: data.get(&p.profile_id).map(stat_tabs).unwrap_or_default(),
                };
                self.put(
                    sink,
                    &format!("{game}/profiles/{name}.json"),
                    &profile,
                    held,
                )?;
                self.counts.profiles += 1;
                let garage = PlayerGarage {
                    cars: garages
                        .remove(&p.profile_id)
                        .map(|rows| garage_cars(&rows))
                        .unwrap_or_default(),
                    creations: own.remove(&p.profile_id).unwrap_or_default(),
                };
                for g in &garage.cars {
                    owners.entry(g.car.id).or_default().push(CarOwner {
                        name: p.name.clone(),
                        races: g.races,
                        ref_: c.ref_.clone(),
                    });
                }
                if !garage.cars.is_empty() || !garage.creations.is_empty() {
                    self.put(
                        sink,
                        &format!("{game}/profiles/{name}.garage.json"),
                        &garage,
                        held,
                    )?;
                    self.counts.garages += 1;
                }
                cards.push(c);
            }
        }
        self.counts.players += cards.len() as u64;
        self.put(
            sink,
            &format!("{base}/players.json"),
            &SnapshotPlayers { rows: cards },
            held,
        )?;
        self.put(
            sink,
            &format!("{base}/cars.json"),
            &catalog_file(owners),
            held,
        )?;
        drop(ranks);
        // Clubs (§6): the game's ClubData statement and the grouped roster, two statements.
        let list = db_clubs::public_clubs_in(conn, game, mode, None).await?;
        let roster = db_clubs::mode_members_in(conn, game, mode, None).await?;
        let mut by_club: HashMap<i32, Vec<db_clubs::ModeMember>> = HashMap::new();
        for m in roster {
            by_club.entry(m.club_id).or_default().push(m);
        }
        let mut cards = Vec::with_capacity(list.len());
        for club in &list {
            // The database holds tags to [A-Z0-9]{2,4}; anything else would be a path.
            if !(2..=4).contains(&club.tag.len())
                || !club
                    .tag
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                return Err(SnapshotError::Write(
                    "a club tag outside [A-Z0-9]{2,4}".into(),
                ));
            }
            let members = by_club.remove(&club.id).unwrap_or_default();
            let Some(page) = club_page(club, &members, &tops, Some(self.refs)) else {
                continue;
            };
            self.put(
                sink,
                &format!("{base}/clubs/{}.json", club.tag),
                &page,
                held,
            )?;
            self.counts.clubs += 1;
            let by_profile = members.iter().map(|m| (m.profile_id, m)).collect();
            cards.push(club_card(club, &by_profile, Some(self.refs)));
        }
        self.put(
            sink,
            &format!("{base}/clubs.json"),
            &ClubList { rows: cards },
            0,
        )?;
        Ok(())
    }
}

impl Snapshotter<'_> {
    /// Forza Motorsport 5 (§5.3, CR-S109): `fm5/players.json` (every listed career, any mode)
    /// and `fm5/profiles/<name>.json` ([`Fm5Career`]). Returns the modes present among the listed
    /// careers, in `modes` order (the manifest's `games.fm5.modes`). Two statements.
    pub async fn fm5_in(
        &mut self,
        conn: &mut PgConnection,
        modes: &[&str],
        sink: &mut dyn Sink,
    ) -> Result<Vec<String>, SnapshotError> {
        let game = fi_domain::FM5;
        let careers = db_players::every_career_in(conn, game).await?;
        let ids: Vec<Uuid> = careers.iter().map(|(c, _)| c.profile_id).collect();
        let mut bests: HashMap<Uuid, Vec<Fm5Best>> = HashMap::new();
        for chunk in ids.chunks(DATA_CHUNK) {
            for b in db_players::fm5_bests_in(conn, chunk).await? {
                bests.entry(b.profile_id).or_default().push(fm5_best(&b));
            }
        }
        let mut cards = Vec::with_capacity(careers.len());
        let mut present: HashSet<String> = HashSet::new();
        for (row, progress) in &careers {
            let name = row.name.to_lowercase();
            if !self.names.insert(format!("{game}/{name}")) {
                return Err(SnapshotError::DuplicateProfile);
            }
            let progress = fm5_progress(progress);
            let mut c = card(row);
            // FM5's level and XP as the game sent them (never FH2's curve).
            c.level = progress.level;
            c.xp = progress.xp;
            c.ref_ = Some(self.refs.of(row.profile_id));
            let career = Fm5Career {
                card: c.clone(),
                since: at(row.since),
                progress,
                bests: bests.remove(&row.profile_id).unwrap_or_default(),
            };
            self.put(sink, &format!("{game}/profiles/{name}.json"), &career, 0)?;
            self.counts.careers += 1;
            present.insert(row.mode.clone());
            cards.push(c);
        }
        self.counts.players += cards.len() as u64;
        self.put(
            sink,
            &format!("{game}/players.json"),
            &SnapshotPlayers { rows: cards },
            0,
        )?;
        Ok(modes
            .iter()
            .filter(|m| present.contains(**m))
            .map(|m| (*m).to_owned())
            .collect())
    }
}

/// The stored progress, numbers outside the wire's range dropped.
fn fm5_progress(p: &db_players::Progress) -> Fm5Progress {
    Fm5Progress {
        level: p.level.and_then(|v| u32::try_from(v).ok()),
        xp: p.xp.and_then(|v| u64::try_from(v).ok()),
        credits: p.credits.and_then(|v| u64::try_from(v).ok()),
        cars: p.cars.and_then(|v| u32::try_from(v).ok()),
    }
}

fn fm5_best(b: &db_players::Fm5BestRow) -> Fm5Best {
    Fm5Best {
        kind: if b.score_type == fi_db::fm5::DRIFT {
            "drift"
        } else {
            "time"
        }
        .to_owned(),
        scoreboard_type: b.scoreboard_type,
        game_scoreboard_id: b.game_scoreboard_id,
        track_id: b.track_id,
        car_id: b.car_id,
        car_class_id: i32::from(b.car_class_id),
        car_drive_type_id: i32::from(b.car_drive_type_id),
        score: f64::from(b.score),
        clean: b.is_clean,
        at: at(b.submitted_at),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<(String, usize)>, u64);

    impl Sink for Mem {
        fn put(&mut self, rel: &str, bytes: Vec<u8>) -> Result<(), String> {
            self.1 += bytes.len() as u64;
            self.0.push((rel.to_owned(), bytes.len()));
            Ok(())
        }
        fn bytes(&self) -> u64 {
            self.1
        }
    }

    #[test]
    fn the_size_guard_counts_what_is_held_too() {
        let refs = RefKey::new("k").unwrap();
        let s = Snapshotter::new(&refs, 100);
        let mut sink = Mem(Vec::new(), 0);
        s.put(&mut sink, "a.json", &"x".repeat(10), 0).unwrap();
        assert!(matches!(
            s.put(&mut sink, "b.json", &"x".repeat(10), 90),
            Err(SnapshotError::TooLarge(100))
        ));
        assert_eq!(sink.0.len(), 1);
    }

    #[test]
    fn refs_are_opaque_stable_and_keyed() {
        let a = RefKey::new(" secret ").unwrap();
        let b = RefKey::new("other").unwrap();
        let id = Uuid::from_u128(0x0192_f0c4_0000_7000_8000_0000_0000_0001);
        let r = a.of(id);
        assert_eq!(r.len(), fi_protocol::dashboard::REF_LENGTH);
        assert!(
            r.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_eq!(
            r,
            RefKey::new("secret").unwrap().of(id),
            "the key is trimmed"
        );
        assert_ne!(r, b.of(id));
        assert_ne!(r, a.of(Uuid::from_u128(2)));
        assert!(!r.contains(&id.simple().to_string()[..8]));
        // The definition (v2): base64url(HMAC-SHA256(ref_key, "fi-dashboard-ref-v2:" || id)), 22
        // characters, ref_key = HMAC-SHA256(key, "fi-dashboard-subkey-v2:ref").
        let ref_key = fi_auth::hmac_sha256(b"secret", b"fi-dashboard-subkey-v2:ref");
        let mac = fi_auth::hmac_sha256(
            &ref_key,
            format!("fi-dashboard-ref-v2:{}", id.hyphenated()).as_bytes(),
        );
        assert_eq!(r, fi_auth::b64::url_encode(&mac)[..22]);
        // Never the v1 value (the raw key, the v1 prefix).
        let v1 = fi_auth::hmac_sha256(
            b"secret",
            format!("fi-dashboard-ref-v1:{}", id.hyphenated()).as_bytes(),
        );
        assert_ne!(r, fi_auth::b64::url_encode(&v1)[..22]);
        assert!(RefKey::new("  ").is_none());
    }
}
