//! The FM5SAVE1 bundle: one Forza Motorsport 5 career packed into one file (M11, OD-219,
//! register CR-S105; SAVES_API.md §4.12, CLOUD_SAVES.md §3.4; the format:
//! `deprecated/beta/notes/local-docs/re/fm5-save-format.md`).
//!
//! An FM5 career is not one Azimuth revision: the profile container `User_<XUID hex>` holds
//! `commit`, `PlayerDatabase`, `ProfileData`, `TransactionLogFile` and `VersionFlags` (no
//! `SaveState`), and every car of its garage names two **sibling** containers next to it,
//! `BaseLivery_<carId>_<yyyymmddhhmmss>` (`C_livery`, `header`) and
//! `GarageThumbnail_<carId>_<ts>` (`bigThumb.png`, `smallThumb.png`), each with its own revisions
//! and `commit`. A bundle carries the profile's newest sealed revision and the newest sealed
//! revision of each sibling container the garage names.
//!
//! Layout (little endian):
//!
//! ```text
//! "FM5SAVE1"
//! u32  container count (1 ..= MAX_CONTAINERS)
//! per container:
//!   u16 name length, name   the first is "User" (the profile container; its XUID is never sent),
//!                            then sibling container names in strictly ascending byte order
//!   u64 label               the container's revision folder name read as hex
//!   u8  file count          exactly its kind's files (the profile: 5, or 4 without its
//!                            optional TransactionLogFile, T-1110-FM5TLF)
//!   per file, in its kind's order (FILES_*):
//!     u16 name length, name, u32 raw size, u32 stored size, u8 method (0 stored, 2 the hook's LZ),
//!     stored bytes
//! ```
//!
//! Nothing may follow the last file. Integrity ([`verify`], [`validate`]): the shape (names, order,
//! per-file size windows, totals), then per container every data file matches its line in that
//! container's `commit` (whose `blobs N` counts exactly its data files), PlayerDatabase is the
//! game's `ssmc` + SQLite container, ProfileData is an `ssmc` property tree whose `Main/Level`,
//! `Main/XP` and `Main/Credits` read, and both thumbnails are PNG. The garage's linkage (each
//! `Career_Garage` row's containers present) is **not** checked here: the crate reads no SQLite.
//!
//! The content digest ([`content_digest`]) covers every data file of every container by container
//! name and file name and ignores every `commit` and label, so the same career installed under new
//! revision folders keeps its digest.

use crate::bundle::{self, BundleError, COMMIT_HEADER, Verified};
use crate::model::{Digest, SaveSummary};

/// Bundle magic.
pub const MAGIC: &[u8; 8] = b"FM5SAVE1";
/// The name the profile container has inside a bundle (the folder is `User_<XUID hex>`).
pub const USER: &str = "User";
/// Prefix of a livery container (`BaseLivery_<carId>_<yyyymmddhhmmss>`).
pub const LIVERY_PREFIX: &str = "BaseLivery_";
/// Prefix of a thumbnail container (`GarageThumbnail_<carId>_<yyyymmddhhmmss>`).
pub const THUMBNAIL_PREFIX: &str = "GarageThumbnail_";
/// The profile container's files, in bundle order. All are required except
/// [`OPTIONAL_USER`] (T-1110-FM5TLF): the game writes revisions without it.
pub const FILES_USER: [&str; 5] = [
    "commit",
    "data/PlayerDatabase",
    "data/ProfileData",
    "data/TransactionLogFile",
    "data/VersionFlags",
];
/// The one profile file a revision may lack: FM5 writes profile revisions without a
/// `TransactionLogFile` (a real career's revisions with `blobs 3`, T-1110-FM5TLF; RE note
/// `fm5-multiplayer.md` §7 saw it absent on a new career). Absent stays absent: nothing invents it.
pub const OPTIONAL_USER: &str = "data/TransactionLogFile";
/// [`FILES_USER`] without [`OPTIONAL_USER`], the profile's files when it is absent.
pub const FILES_USER_NO_LOG: [&str; 4] = [
    "commit",
    "data/PlayerDatabase",
    "data/ProfileData",
    "data/VersionFlags",
];
/// A livery container's files, in bundle order; all required.
pub const FILES_LIVERY: [&str; 3] = ["commit", "data/C_livery", "data/header"];
/// A thumbnail container's files, in bundle order; all required.
pub const FILES_THUMBNAIL: [&str; 3] = ["commit", "data/bigThumb.png", "data/smallThumb.png"];
/// Most sibling containers in one bundle: 200 cars, each with a livery and a thumbnail.
pub const MAX_SIBLINGS: usize = 400;
/// Most containers in one bundle (the profile and [`MAX_SIBLINGS`]).
pub const MAX_CONTAINERS: usize = 1 + MAX_SIBLINGS;
/// Largest bundle accepted: 48 MiB. A car costs about 230 KB (its thumbnails are PNG and do not
/// pack), the profile about 40 KB packed, so [`MAX_SIBLINGS`] fits with room.
pub const MAX_BUNDLE: usize = 48 << 20;
/// Most bytes unpacked in one bundle.
pub const MAX_UNPACKED: u64 = 64 << 20;
/// `(name, smallest, largest)` unpacked size of each file. Seen on real saves (RE note §1):
/// PlayerDatabase 6291464 and ProfileData 20488 always, TransactionLogFile 8 or 33,
/// VersionFlags 4, C_livery 90-102, header 124-130, bigThumb 182-207 KB, smallThumb 17-20 KB; the
/// windows leave room for what the lab has not seen (a painted livery, a larger garage).
pub const FILE_SIZES: [(&str, u32, u32); 9] = [
    ("commit", 64, 2048),
    ("data/PlayerDatabase", 1 << 20, 8 << 20),
    ("data/ProfileData", 4 << 10, 64 << 10),
    ("data/TransactionLogFile", 8, 64 << 10),
    ("data/VersionFlags", 4, 4),
    ("data/C_livery", 1, 256 << 10),
    ("data/header", 1, 4 << 10),
    ("data/bigThumb.png", 1 << 10, 512 << 10),
    ("data/smallThumb.png", 256, 128 << 10),
];
const PNG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// What a container is, from its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The profile container (`User_<XUID hex>`).
    User,
    /// `BaseLivery_<carId>_<ts>`.
    Livery,
    /// `GarageThumbnail_<carId>_<ts>`.
    Thumbnail,
}

impl Kind {
    /// The container's files, in bundle order.
    pub fn files(self) -> &'static [&'static str] {
        match self {
            Kind::User => &FILES_USER,
            Kind::Livery => &FILES_LIVERY,
            Kind::Thumbnail => &FILES_THUMBNAIL,
        }
    }

    /// The kind of a sibling container's name (`<prefix><carId>_<yyyymmddhhmmss>`, a car id of 1
    /// to 10 digits), `None` for any other name (the profile's included).
    pub fn of_sibling(name: &str) -> Option<Kind> {
        let (kind, rest) = if let Some(rest) = name.strip_prefix(LIVERY_PREFIX) {
            (Kind::Livery, rest)
        } else {
            (Kind::Thumbnail, name.strip_prefix(THUMBNAIL_PREFIX)?)
        };
        let (car, stamp) = rest.split_once('_')?;
        let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        ((1..=10).contains(&car.len()) && digits(car) && stamp.len() == 14 && digits(stamp))
            .then_some(kind)
    }
}

/// One file of a container, before unpacking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Its name (one of its kind's files).
    pub name: &'static str,
    /// Unpacked size.
    pub raw_size: u32,
    /// Packing method (0 or 2).
    pub method: u8,
    /// Stored bytes.
    pub stored: &'a [u8],
}

/// One container of a bundle, before unpacking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerLayout<'a> {
    /// [`USER`] or the sibling folder's name.
    pub name: &'a str,
    /// Its kind.
    pub kind: Kind,
    /// Its revision label (the folder name as a number).
    pub label: u64,
    /// Its files, in its kind's order.
    pub entries: Vec<Entry<'a>>,
}

/// One container, unpacked: what [`pack`] takes and [`validate`] returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    /// [`USER`] or the sibling folder's name.
    pub name: String,
    /// Its revision label (Azimuth's revision folder name read as hex).
    pub label: u64,
    /// `(file name, bytes)` in its kind's order (`commit` first).
    pub files: Vec<(&'static str, Vec<u8>)>,
}

impl Container {
    /// The label as Azimuth names the folder: 16 upper-case hex digits.
    pub fn label_hex(&self) -> String {
        format!("{:016X}", self.label)
    }

    /// The bytes of one file.
    pub fn file(&self, name: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| b.as_slice())
    }
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BundleError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&e| e <= self.data.len())
            .ok_or(BundleError::Truncated)?;
        let slice = &self.data[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn u16(&mut self) -> Result<u16, BundleError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, BundleError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, BundleError> {
        let mut a = [0u8; 8];
        a.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(a))
    }
}

fn shown(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).chars().take(40).collect()
}

/// Parses and checks a bundle's shape without unpacking.
pub fn parse_layout(data: &[u8]) -> Result<Vec<ContainerLayout<'_>>, BundleError> {
    if data.len() > MAX_BUNDLE {
        return Err(BundleError::TooLarge);
    }
    let mut r = Reader { data, at: 0 };
    if r.take(8)? != MAGIC {
        return Err(BundleError::UnknownFormat);
    }
    let count = r.u32()? as usize;
    if count == 0 || count > MAX_CONTAINERS {
        return Err(BundleError::InvalidContainer(format!("{count} containers")));
    }
    let mut containers: Vec<ContainerLayout<'_>> = Vec::with_capacity(count.min(64));
    let mut unpacked = 0u64;
    for i in 0..count {
        let len = r.u16()? as usize;
        let raw = r.take(len)?;
        let name =
            std::str::from_utf8(raw).map_err(|_| BundleError::InvalidContainer(shown(raw)))?;
        let kind = if i == 0 {
            (name == USER)
                .then_some(Kind::User)
                .ok_or_else(|| BundleError::InvalidContainer(shown(raw)))?
        } else {
            let kind =
                Kind::of_sibling(name).ok_or_else(|| BundleError::InvalidContainer(shown(raw)))?;
            // Strictly ascending: canonical, and no container twice.
            if containers.len() > 1 && containers[containers.len() - 1].name.as_bytes() >= raw {
                return Err(BundleError::InvalidContainer(shown(raw)));
            }
            kind
        };
        let label = r.u64()?;
        let count = r.take(1)?[0] as usize;
        let files: &'static [&'static str] = match kind {
            Kind::User if count == FILES_USER_NO_LOG.len() => &FILES_USER_NO_LOG,
            _ => kind.files(),
        };
        if count != files.len() {
            return Err(BundleError::InvalidFileCount);
        }
        let mut entries = Vec::with_capacity(files.len());
        for want in files.iter().copied() {
            let name_len = r.u16()? as usize;
            let raw_name = r.take(name_len)?;
            // Its kind's files, each once, in order: anything else (a missing, repeated, foreign or
            // reordered file) is the first name that is not the expected one.
            if raw_name != want.as_bytes() {
                return Err(BundleError::UnexpectedFile(shown(raw_name)));
            }
            let raw_size = r.u32()?;
            let stored_size = r.u32()?;
            let method = r.take(1)?[0];
            if !matches!(method, 0 | 2)
                || (method == 0 && stored_size != raw_size)
                || stored_size > raw_size
            {
                return Err(BundleError::InvalidFile(want));
            }
            if let Some((_, min, max)) = FILE_SIZES.iter().find(|(n, _, _)| *n == want)
                && !(*min..=*max).contains(&raw_size)
            {
                return Err(BundleError::SizeOutOfRange {
                    name: want,
                    size: raw_size,
                });
            }
            unpacked += u64::from(raw_size);
            if unpacked > MAX_UNPACKED {
                return Err(BundleError::TooLarge);
            }
            entries.push(Entry {
                name: want,
                raw_size,
                method,
                stored: r.take(stored_size as usize)?,
            });
        }
        containers.push(ContainerLayout {
            name,
            kind,
            label,
            entries,
        });
    }
    if r.at != data.len() {
        return Err(BundleError::TrailingData);
    }
    Ok(containers)
}

fn decode(entry: &Entry<'_>) -> Result<Vec<u8>, BundleError> {
    let size = entry.raw_size as usize;
    match entry.method {
        0 => Some(entry.stored.to_vec()),
        2 => bundle::lz_decode(entry.stored, size),
        _ => None,
    }
    .filter(|b| b.len() == size)
    .ok_or(BundleError::Unpack(entry.name))
}

/// The `commit` of one container lists exactly its data files: Azimuth's header, `blobs N` with
/// N its data files, and a `<sha256> <size> <name>` line for each (any order).
fn commit_lists(commit: &str, files: &[(&'static str, Digest, usize)]) -> Option<&'static str> {
    let lines: Vec<&str> = commit
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.is_empty())
        .collect();
    let blobs = format!("blobs {}", files.len());
    for (name, sha, size) in files {
        let short = name.trim_start_matches("data/");
        let line = format!("{} {size} {short}", sha.hex());
        if !lines.iter().skip(2).any(|l| *l == line) {
            return Some(name);
        }
    }
    if lines.first() != Some(&COMMIT_HEADER)
        || lines.get(1) != Some(&blobs.as_str())
        || lines.len() != 2 + files.len()
    {
        return Some("commit");
    }
    None
}

fn is_game_database(body: &[u8]) -> bool {
    let inner = body
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .unwrap_or(0);
    body.get(..4) == Some(b"ssmc".as_slice())
        && inner + 8 <= body.len()
        && body.get(8..24) == Some(b"SQLite format 3\0".as_slice())
}

/// The progress numbers of an FM5 ProfileData (RE note §4): `Main/Credits`, `Main/XP`,
/// `Main/Level` and `Main/TotalWinnings`, each by path in the property tree, `None` where it does
/// not read. `level` is `Main/Level` as stored (FM5 has no game rules on the server: it is kept as
/// read). `cars` is not here (see [`summary`]).
pub fn profile_summary(profile: &[u8]) -> SaveSummary {
    let read = |name: &str| bundle::profile_path_int(profile, &format!("Main/{name}"));
    SaveSummary {
        credits: read("Credits"),
        xp: read("XP"),
        level: read("Level"),
        winnings: read("TotalWinnings"),
        ..SaveSummary::default()
    }
}

/// Whether ProfileData is the game's property tree: the `ssmc` header and a payload length inside
/// the file, and `Main/Level`, `Main/XP` and `Main/Credits` read.
fn is_profile_data(body: &[u8]) -> bool {
    let inner = body
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .unwrap_or(usize::MAX);
    let s = profile_summary(body);
    body.get(..4) == Some(b"ssmc".as_slice())
        && inner.checked_add(8).is_some_and(|n| n <= body.len())
        && s.level.is_some()
        && s.xp.is_some()
        && s.credits.is_some()
}

/// The content digest: SHA-256 over every data file of every container, in bundle order, as
/// `container || 0x00 || file || 0x00 || sha256(bytes)`. Commits and labels are left out.
pub fn content_digest(containers: &[Container]) -> Digest {
    let mut text = Vec::new();
    for c in containers {
        for (name, bytes) in c.files.iter().filter(|(n, _)| *n != "commit") {
            digest_line(&mut text, &c.name, name, &Digest::of(bytes));
        }
    }
    Digest::of(&text)
}

fn digest_line(text: &mut Vec<u8>, container: &str, file: &str, sha: &Digest) {
    text.extend_from_slice(container.as_bytes());
    text.push(0);
    text.extend_from_slice(file.as_bytes());
    text.push(0);
    text.extend_from_slice(&sha.0);
}

/// The bundle's progress numbers: [`profile_summary`] of ProfileData, and `cars` the number of
/// livery containers (one per `Career_Garage` row, `Career_Garage.LiveryFileName`; INFERRED from
/// the lab's careers, RE note §1).
pub fn summary(containers: &[Container]) -> SaveSummary {
    let profile = containers
        .first()
        .and_then(|c| c.file("data/ProfileData"))
        .unwrap_or(&[]);
    SaveSummary {
        cars: Some(livery_count(containers.iter().map(|c| c.name.as_str()))),
        ..profile_summary(profile)
    }
}

fn livery_count<'a>(names: impl Iterator<Item = &'a str>) -> i64 {
    names.filter(|n| n.starts_with(LIVERY_PREFIX)).count() as i64
}

/// A bundle that passed every check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidBundle {
    /// Its containers, unpacked: the profile first.
    pub containers: Vec<Container>,
    /// Its content digest.
    pub content: Digest,
    /// Its progress numbers.
    pub summary: SaveSummary,
}

impl ValidBundle {
    /// The profile container's label.
    pub fn label(&self) -> u64 {
        self.containers.first().map_or(0, |c| c.label)
    }
}

/// The checks of both [`validate`] and [`verify`], one file unpacked at a time; `keep` keeps the
/// unpacked files. An unpack failure anywhere wins over a content failure (as FH2's), and the first
/// content failure in bundle order is the one reported.
fn check(data: &[u8], keep: bool) -> Result<(Vec<Container>, Digest, SaveSummary), BundleError> {
    let layout = parse_layout(data)?;
    let mut digests = Vec::new();
    let mut failure: Option<BundleError> = None;
    let mut summary = SaveSummary::default();
    let mut kept = Vec::new();
    for c in &layout {
        let commit = decode(&c.entries[0])?;
        let mut listed = Vec::with_capacity(c.entries.len());
        let mut files: Vec<(&'static str, Vec<u8>)> = Vec::new();
        for e in &c.entries[1..] {
            let body = decode(e)?;
            let sha = Digest::of(&body);
            let content = match e.name {
                "data/PlayerDatabase" if !is_game_database(&body) => {
                    Some(BundleError::NotGameDatabase)
                }
                "data/ProfileData" => {
                    summary = profile_summary(&body);
                    (!is_profile_data(&body)).then_some(BundleError::NotProfileData)
                }
                "data/bigThumb.png" | "data/smallThumb.png" if !body.starts_with(PNG) => {
                    Some(BundleError::NotThumbnail(e.name))
                }
                _ => None,
            };
            listed.push((e.name, sha, body.len()));
            digest_line(&mut digests, c.name, e.name, &sha);
            if keep {
                files.push((e.name, body));
            }
            if failure.is_none() {
                failure = content;
            }
        }
        if failure.is_none()
            && let Some(name) = commit_lists(&String::from_utf8_lossy(&commit), &listed)
        {
            failure = Some(BundleError::CommitMismatch(name));
        }
        if keep {
            files.insert(0, ("commit", commit));
            kept.push(Container {
                name: c.name.to_owned(),
                label: c.label,
                files,
            });
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    summary.cars = Some(livery_count(layout.iter().map(|c| c.name)));
    Ok((kept, Digest::of(&digests), summary))
}

/// Shape, unpacking and content checks, then digest and summary, keeping the unpacked files.
pub fn validate(data: &[u8]) -> Result<ValidBundle, BundleError> {
    let (containers, content, summary) = check(data, true)?;
    Ok(ValidBundle {
        containers,
        content,
        summary,
    })
}

/// [`validate`] for the server's upload path: the same checks and results, keeping no file bytes
/// (peak memory is the largest file). `label` is the profile container's.
pub fn verify(data: &[u8]) -> Result<Verified, BundleError> {
    let label = parse_layout(data)?.first().map_or(0, |c| c.label);
    let (_, content, summary) = check(data, false)?;
    Ok(Verified {
        label,
        content,
        summary,
    })
}

/// Packs containers into an FM5SAVE1 bundle: method 2 where it is smaller, else stored. Pass the
/// profile first, then the siblings in ascending name order, each container's files in its kind's
/// order, for a bundle [`validate`] accepts.
pub fn pack(containers: &[Container]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&(containers.len() as u32).to_le_bytes());
    for c in containers {
        out.extend_from_slice(&(c.name.len() as u16).to_le_bytes());
        out.extend_from_slice(c.name.as_bytes());
        out.extend_from_slice(&c.label.to_le_bytes());
        out.push(c.files.len() as u8);
        for (name, data) in &c.files {
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            let packed = if data.is_empty() {
                Vec::new()
            } else {
                bundle::lz_encode(data)
            };
            let (method, body) = if !packed.is_empty() && packed.len() < data.len() {
                (2u8, &packed)
            } else {
                (0u8, data)
            };
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.push(method);
            out.extend_from_slice(body);
        }
    }
    out
}

/// Azimuth's `commit` for a container's data files (`(name, bytes)`, `commit` excluded): the header,
/// `blobs N` and one `<sha256> <size> <name>` line each, as the store writes it.
pub fn commit_for(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let data: Vec<_> = files.iter().filter(|(n, _)| *n != "commit").collect();
    let mut text = format!("{COMMIT_HEADER}\nblobs {}\n", data.len());
    for (name, bytes) in data {
        text.push_str(&format!(
            "{} {} {}\n",
            Digest::of(bytes).hex(),
            bytes.len(),
            name.trim_start_matches("data/")
        ));
    }
    text.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_names() {
        assert_eq!(
            Kind::of_sibling("BaseLivery_1426_20261108123456"),
            Some(Kind::Livery)
        );
        assert_eq!(
            Kind::of_sibling("GarageThumbnail_7_20261108123456"),
            Some(Kind::Thumbnail)
        );
        for bad in [
            "User",
            "Replay_1426_20261108123456",
            "BaseLivery_1426_2026110812345",
            "BaseLivery__20261108123456",
            "BaseLivery_12345678901_20261108123456",
            "BaseLivery_14x6_20261108123456",
            "GarageThumbnail_1426_20261108123456_",
            "baselivery_1426_20261108123456",
        ] {
            assert_eq!(Kind::of_sibling(bad), None, "{bad}");
        }
    }
}
