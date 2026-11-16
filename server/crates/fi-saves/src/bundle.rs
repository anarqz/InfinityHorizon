//! The FH2SAVE1 bundle: one Azimuth save revision packed into one file (CLOUD_SAVES.md §3).
//!
//! Ported from ForzaInfinityBeta `server/src/db/saves.rs` (layout and shape checks),
//! `server/src/db/reset.rs` (the hook's LZ encoder, `pack`, ProfileData ints) and
//! `server/src/db/save_bests.rs` (the decoders), which match `hook/adapters/azimuth_codec.c`.
//! Server and launcher share this module, so both sides pack, unpack and validate identically.
//!
//! Layout (little endian): `"FH2SAVE1"`, `u64` label (Azimuth's revision folder name read as hex),
//! `u32` file count, then per file: `u16` name length, name (one of [`SAVE_FILES`]), `u32` raw
//! size, `u32` stored size, `u8` method (0 stored, 1 XPRESS Huffman from pre-method-2 hooks on
//! Windows, 2 the hook's LZ), stored bytes.
//!
//! [`validate`] is the integrity check every upload passes before anything is stored: the shape
//! (names, per-file size windows of real saves, totals, no trailing bytes), then the contents
//! (every file unpacks to its declared size and matches its SHA-256 and size line in `commit`,
//! PlayerDatabase is the game's `ssmc` container around a SQLite file, SaveState is XML). It also
//! computes the [`content_digest`] and the progress [`SaveSummary`] read from ProfileData.
//! [`verify`] runs the same checks for the server's upload path without keeping the unpacked
//! files (one file in memory at a time, each hashed once).

use crate::model::{Digest, SaveSummary};
use std::fmt;

/// Bundle magic.
pub const MAGIC: &[u8; 8] = b"FH2SAVE1";
/// First line of every Azimuth commit file; then `blobs N` and N `<sha256> <size> <name>` lines.
pub const COMMIT_HEADER: &str = "Azimuth complete save revision v2";
/// The files of an Azimuth save revision, the only names a bundle may hold, in canonical order.
pub const SAVE_FILES: [&str; 7] = [
    "commit",
    "data/BinarySaveState",
    "data/PlayerDatabase",
    "data/ProfileData",
    "data/SaveState",
    "data/TransactionLogFile",
    "data/VersionFlags",
];
/// Files every save has.
pub const REQUIRED: [&str; 4] = [
    "commit",
    "data/PlayerDatabase",
    "data/ProfileData",
    "data/SaveState",
];
/// Largest bundle accepted: a real save packs to about 28 KB (6.5 MB unpacked, mostly zeros).
pub const MAX_BUNDLE: usize = 384 << 10;
/// `(name, smallest, largest)` unpacked size of each file, from real saves (Beta `FILE_SIZES`).
/// The minimums of `BinarySaveState` (4 B) and `SaveState` (1 KiB) were lowered for online-profile
/// revisions (launcher CR-L6); a revision read mid-write is still caught by the `commit` line
/// check of every file.
pub const FILE_SIZES: [(&str, u32, u32); 7] = [
    ("commit", 256, 2048),
    ("data/BinarySaveState", 4, 256 << 10),
    ("data/PlayerDatabase", 6_291_464, 6_291_464),
    ("data/ProfileData", 20_488, 20_488),
    ("data/SaveState", 1 << 10, 256 << 10),
    ("data/TransactionLogFile", 0, 64 << 10),
    ("data/VersionFlags", 4, 4),
];
const MAX_FILE: u32 = 8 << 20;
const MAX_UNPACKED: u64 = 16 << 20;
const LZ_MIN_MATCH: usize = 4;
const LZ_HASH_BITS: u32 = 16;

/// Why a bundle is not a valid FH2 save. Never a reason for a strike: these are almost always a
/// client reading a revision mid-write or a broken file, not cheating (Beta failure 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleError {
    /// Larger than [`MAX_BUNDLE`] or more than 16 MB unpacked.
    TooLarge,
    /// Ends in the middle of a field.
    Truncated,
    /// Wrong magic.
    UnknownFormat,
    /// Zero files, or more than the seven a save has.
    InvalidFileCount,
    /// A name outside [`SAVE_FILES`], or one listed twice.
    UnexpectedFile(String),
    /// Impossible sizes or method for one file.
    InvalidFile(&'static str),
    /// A file whose unpacked size is outside the window real saves have.
    SizeOutOfRange {
        /// File name.
        name: &'static str,
        /// Declared size.
        size: u32,
    },
    /// A required file is absent.
    MissingFile(&'static str),
    /// Bytes after the last file.
    TrailingData,
    /// A file does not unpack to its declared size.
    Unpack(&'static str),
    /// A file does not match its line in `commit` (SHA-256 and size).
    CommitMismatch(&'static str),
    /// PlayerDatabase is not the game's `ssmc` + SQLite container.
    NotGameDatabase,
    /// SaveState is not XML.
    NotXml,
    /// FM5SAVE1 only ([`crate::fm5`]): a container that is not the profile first, a sibling name
    /// that is not a livery's or a thumbnail's, out of order or twice, or too many containers.
    InvalidContainer(String),
    /// FM5SAVE1 only: ProfileData is not the game's property tree (`Main/Level`, `Main/XP` and
    /// `Main/Credits` do not read).
    NotProfileData,
    /// FM5SAVE1 only: a thumbnail that is not a PNG.
    NotThumbnail(&'static str),
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BundleError::TooLarge => f.write_str("save too big"),
            BundleError::Truncated => f.write_str("truncated bundle"),
            BundleError::UnknownFormat => f.write_str("unknown save format"),
            BundleError::InvalidFileCount => f.write_str("invalid file count"),
            BundleError::UnexpectedFile(name) => write!(f, "unexpected file in the save: {name}"),
            BundleError::InvalidFile(name) => write!(f, "invalid save file {name}"),
            BundleError::SizeOutOfRange { name, size } => {
                write!(f, "{name}: {size} bytes is not a real save's size")
            }
            BundleError::MissingFile(name) => write!(f, "save without {name}"),
            BundleError::TrailingData => f.write_str("trailing data in the bundle"),
            BundleError::Unpack(name) => write!(f, "{name} does not unpack"),
            BundleError::CommitMismatch(name) => {
                write!(f, "{name} does not match the save's commit")
            }
            BundleError::NotGameDatabase => {
                f.write_str("PlayerDatabase is not the game's database")
            }
            BundleError::NotXml => f.write_str("SaveState is not XML"),
            BundleError::InvalidContainer(name) => write!(f, "invalid save container {name}"),
            BundleError::NotProfileData => f.write_str("ProfileData is not the game's profile"),
            BundleError::NotThumbnail(name) => write!(f, "{name} is not a PNG"),
        }
    }
}

impl std::error::Error for BundleError {}

/// One file of a bundle, before unpacking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
    /// Its name (one of [`SAVE_FILES`]).
    pub name: &'static str,
    /// Unpacked size.
    pub raw_size: u32,
    /// Packing method.
    pub method: u8,
    /// Stored bytes.
    pub stored: &'a [u8],
}

/// A bundle's parsed layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout<'a> {
    /// The label: Azimuth's revision folder name as a number.
    pub label: u64,
    /// Its files, in bundle order.
    pub entries: Vec<Entry<'a>>,
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
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
}

/// Parses and checks a bundle's shape without unpacking (Beta `check_bundle`).
pub fn parse_layout(data: &[u8]) -> Result<Layout<'_>, BundleError> {
    if data.len() > MAX_BUNDLE {
        return Err(BundleError::TooLarge);
    }
    let mut r = Reader { data, at: 0 };
    if r.take(8)? != MAGIC {
        return Err(BundleError::UnknownFormat);
    }
    let label = r.u64()?;
    let count = r.u32()?;
    if count == 0 || count as usize > SAVE_FILES.len() {
        return Err(BundleError::InvalidFileCount);
    }
    let mut entries: Vec<Entry<'_>> = Vec::with_capacity(count as usize);
    let mut unpacked = 0u64;
    for _ in 0..count {
        let name_len = r.u16()? as usize;
        let raw_name = r.take(name_len)?;
        let shown = String::from_utf8_lossy(raw_name)
            .chars()
            .take(40)
            .collect::<String>();
        let name = SAVE_FILES
            .iter()
            .copied()
            .find(|n| n.as_bytes() == raw_name)
            .ok_or_else(|| BundleError::UnexpectedFile(shown.clone()))?;
        if entries.iter().any(|e| e.name == name) {
            return Err(BundleError::UnexpectedFile(shown));
        }
        let raw_size = r.u32()?;
        let stored_size = r.u32()?;
        let method = r.take(1)?[0];
        // A packed file never takes more room than it unpacks to (the hook stores it as is then).
        if raw_size > MAX_FILE
            || method > 2
            || (method == 0 && stored_size != raw_size)
            || stored_size > raw_size
        {
            return Err(BundleError::InvalidFile(name));
        }
        if let Some((_, min, max)) = FILE_SIZES.iter().find(|(n, _, _)| *n == name)
            && !(*min..=*max).contains(&raw_size)
        {
            return Err(BundleError::SizeOutOfRange {
                name,
                size: raw_size,
            });
        }
        unpacked += u64::from(raw_size);
        if unpacked > MAX_UNPACKED {
            return Err(BundleError::TooLarge);
        }
        let stored = r.take(stored_size as usize)?;
        entries.push(Entry {
            name,
            raw_size,
            method,
            stored,
        });
    }
    if let Some(missing) = REQUIRED
        .iter()
        .find(|req| !entries.iter().any(|e| e.name == **req))
    {
        return Err(BundleError::MissingFile(missing));
    }
    if r.at != data.len() {
        return Err(BundleError::TrailingData);
    }
    Ok(Layout { label, entries })
}

/// Unpacks every file, in [`SAVE_FILES`] order.
pub fn unpack(layout: &Layout<'_>) -> Result<Vec<(&'static str, Vec<u8>)>, BundleError> {
    let mut files = Vec::with_capacity(layout.entries.len());
    for name in SAVE_FILES {
        let Some(entry) = layout.entries.iter().find(|e| e.name == name) else {
            continue;
        };
        files.push((name, decode(entry)?));
    }
    Ok(files)
}

fn file<'a>(files: &'a [(&'static str, Vec<u8>)], name: &str) -> Option<&'a [u8]> {
    files
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| b.as_slice())
}

/// Checks unpacked files against `commit` and the game's formats (Beta `check_contents`).
pub fn check_contents(files: &[(&'static str, Vec<u8>)]) -> Result<(), BundleError> {
    let commit =
        String::from_utf8_lossy(file(files, "commit").ok_or(BundleError::MissingFile("commit"))?)
            .into_owned();
    for (name, body) in files.iter().filter(|(n, _)| *n != "commit") {
        let short = name.trim_start_matches("data/");
        let line = format!("{} {} {short}", Digest::of(body).hex(), body.len());
        if !commit.lines().any(|l| l.trim_end_matches('\r') == line) {
            return Err(BundleError::CommitMismatch(name));
        }
    }
    let db = file(files, "data/PlayerDatabase").unwrap_or(&[]);
    let inner = db
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
        .unwrap_or(0);
    if db.get(..4) != Some(b"ssmc".as_slice())
        || inner + 8 > db.len()
        || db.get(8..24) != Some(b"SQLite format 3\0".as_slice())
    {
        return Err(BundleError::NotGameDatabase);
    }
    let state = file(files, "data/SaveState").unwrap_or(&[]);
    let first = state
        .iter()
        .find(|b| !(b.is_ascii_whitespace() || matches!(**b, 0xef | 0xbb | 0xbf)));
    if first != Some(&b'<') {
        return Err(BundleError::NotXml);
    }
    Ok(())
}

/// The content digest: SHA-256 over each data file present, in [`SAVE_FILES`] order, as
/// `name || 0x00 || sha256(bytes)`. It ignores `commit` and the bundle label, so the same save
/// installed into a new Azimuth revision folder (the download path) has the same digest. Sync
/// decisions compare this, never bundle hashes (CLOUD_SAVES.md §3.3).
pub fn content_digest(files: &[(&'static str, Vec<u8>)]) -> Digest {
    let mut text = Vec::with_capacity(SAVE_FILES.len() * 64);
    for name in SAVE_FILES.iter().skip(1) {
        if let Some(bytes) = file(files, name) {
            text.extend_from_slice(name.as_bytes());
            text.push(0);
            text.extend_from_slice(&Digest::of(bytes).0);
        }
    }
    Digest::of(&text)
}

/// ProfileData names an int as `u32` length, the name, `" \0\0\0\0\x03\0\0\0"`, then an `i32`
/// (the length keeps "Level" from matching "WristbandLevel").
fn profile_marker(name: &str) -> Vec<u8> {
    let mut m = (name.len() as u32).to_le_bytes().to_vec();
    m.extend_from_slice(name.as_bytes());
    m.extend_from_slice(b" \0\0\0\0\x03\0\0\0");
    m
}

fn find_once(hay: &[u8], needle: &[u8]) -> Option<usize> {
    let mut found = hay
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| i);
    let first = found.next()?;
    found.next().is_none().then_some(first)
}

/// An int of ProfileData; it must appear exactly once.
pub fn profile_int(profile: &[u8], name: &str) -> Option<i32> {
    let m = profile_marker(name);
    let at = find_once(profile, &m)? + m.len();
    let b = profile.get(at..at + 4)?;
    Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// ProfileData with one int replaced (it must appear exactly once). Server-side resets and the
/// fresh career use it (Beta `set_profile_int`).
pub fn set_profile_int(profile: &[u8], name: &str, value: i32) -> Option<Vec<u8>> {
    let m = profile_marker(name);
    let at = find_once(profile, &m)? + m.len();
    profile.get(at..at + 4)?;
    let mut out = profile.to_vec();
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    Some(out)
}

/// The `commit` file with each listed data file's `sha256 size name` line remade from `files`, so
/// an edited save stays consistent (Beta `commit_with`).
pub fn commit_with(commit: &[u8], files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let text = String::from_utf8_lossy(commit);
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| {
            let parts: Vec<&str> = line.split(' ').collect();
            if parts.len() == 3
                && let Some((_, data)) = files
                    .iter()
                    .find(|(n, _)| n.strip_prefix("data/") == Some(parts[2]))
            {
                return format!("{} {} {}", Digest::of(data).hex(), data.len(), parts[2]);
            }
            line.to_owned()
        })
        .collect();
    lines.join("\n").into_bytes()
}

/// The progress numbers a save holds: ProfileData `Main/Credits`, `Main/XP`, `Main/Level`, and for
/// the guardrails (OD-62) `Main/TotalWinnings` (lifetime credits earned) and `Main/NumSkillTokens`
/// (tokens earned), with SaveState's `SkillsState` `SkillPointsTotal` and
/// `UnspentSkillPointsTotal`. `cars` needs PlayerDatabase's SQLite and stays `None` here (the
/// server-side reader lands with SER-M2).
///
/// `level` here is ProfileData `Main/Level` as stored, which is **not** the driver level (the
/// level-ups inside the current colour band; 2026-10-31). This crate holds no game data, so the
/// server replaces it with the level the XP reaches on the game's curve
/// ([`crate::gates::with_driver_level`]) before keeping or measuring a summary.
pub fn summary(files: &[(&'static str, Vec<u8>)]) -> SaveSummary {
    let mut s = profile_summary(file(files, "data/ProfileData").unwrap_or(&[]));
    with_skills(&mut s, file(files, "data/SaveState").unwrap_or(&[]));
    s
}

/// ProfileData's numbers: each of `Main/Credits`, `XP`, `Level`, `TotalWinnings` and
/// `NumSkillTokens` by path where the tree parses (a real save has a second `TotalWinnings`,
/// `Island/`'s), else by the name that appears once with an `i32` marker (synthetic saves are not
/// trees). When both reads find a number and they disagree (a second, typed copy planted for the
/// name read; review G2-10), nothing is readable: the summary is empty, so a Hardcore upload fails
/// closed (`SAVES.SUMMARY_UNREADABLE`, parked).
pub fn profile_summary(profile: &[u8]) -> SaveSummary {
    let read = |name: &str| -> Result<Option<i64>, ()> {
        let by_path = profile_path_int(profile, &format!("Main/{name}"));
        let by_name = profile_int(profile, name).map(i64::from);
        match (by_path, by_name) {
            (Some(p), Some(n)) if p != n => Err(()),
            (p, n) => Ok(p.or(n)),
        }
    };
    let numbers = (|| {
        Ok::<_, ()>(SaveSummary {
            credits: read("Credits")?,
            xp: read("XP")?,
            level: read("Level")?,
            cars: None,
            winnings: read("TotalWinnings")?,
            tokens: read("NumSkillTokens")?,
            skill_points: None,
            unspent_skill_points: None,
        })
    })();
    numbers.unwrap_or_default()
}

fn with_skills(summary: &mut SaveSummary, save_state: &[u8]) {
    let (points, unspent) = save_state_skills(save_state);
    summary.skill_points = points;
    summary.unspent_skill_points = unspent;
}

/// ProfileData's integer at `path` (`Main/TotalWinnings`): its property tree walked from the
/// header (`"ssmc"`, `u32` payload length, `u32` top-level count; per property a `u32` name
/// length, the ASCII name, `" \0\0\0\0"`, a `u32` type and the value; type 0x0f a group of `u32`
/// children). `None` when the tree does not parse, the path is absent or not an integer.
pub fn profile_path_int(profile: &[u8], path: &str) -> Option<i64> {
    let u32_at = |at: usize| -> Option<u32> {
        let b = profile.get(at..at.checked_add(4)?)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    if profile.get(..4) != Some(b"ssmc".as_slice()) {
        return None;
    }
    let end = (u32_at(4)? as usize).checked_add(8)?.min(profile.len());
    let want: Vec<&str> = path.split('/').collect();
    // (position, children left at each depth) walked iteratively; depth bounded by the path.
    let mut at = 12usize;
    let mut left = vec![u32_at(8)?];
    let mut trail: Vec<String> = Vec::new();
    let mut steps = 0u32;
    while let Some(n) = left.last_mut() {
        steps += 1;
        if steps > 10_000 || at >= end {
            return None;
        }
        if *n == 0 {
            left.pop();
            trail.pop();
            continue;
        }
        *n -= 1;
        let len = u32_at(at)? as usize;
        if len == 0 || len > 128 {
            return None;
        }
        let name = std::str::from_utf8(profile.get(at + 4..at + 4 + len)?).ok()?;
        at += 4 + len;
        if profile.get(at..at + 5)? != b" \0\0\0\0" {
            return None;
        }
        let kind = u32_at(at + 5)?;
        at += 9;
        // The property `path` names: in the groups the path names, with its last name.
        let here = trail.len() + 1 == want.len()
            && want.last() == Some(&name)
            && trail.iter().zip(&want).all(|(a, b)| a == b);
        let value = |size: usize| profile.get(at..at + size);
        match kind {
            0 | 1 => {
                if here {
                    return value(1).map(|b| i64::from(b[0]));
                }
                at += 1;
            }
            3 | 7 => {
                if here {
                    let v = u32_at(at)?;
                    return Some(if kind == 3 {
                        i64::from(v as i32)
                    } else {
                        i64::from(v)
                    });
                }
                at += 4;
            }
            9 => at += 4,
            4 => {
                if here {
                    let b = value(8)?;
                    return Some(i64::from_le_bytes(b.try_into().ok()?));
                }
                at += 8;
            }
            0x0c => at += 4 + 2 * u32_at(at)? as usize,
            0x0f => {
                let children = u32_at(at)?;
                at += 4;
                if left.len() > 8 {
                    return None;
                }
                left.push(children);
                trail.push(name.to_owned());
            }
            _ => return None,
        }
    }
    None
}

/// SaveState's `SkillsState` (`<Object ... Type="SkillsState">` with `<Property
/// Name="SkillPointsTotal" Value="..."/>` and `UnspentSkillPointsTotal`): `(points, unspent)`,
/// each `None` when absent or not a number.
pub fn save_state_skills(xml: &[u8]) -> (Option<i64>, Option<i64>) {
    let text = String::from_utf8_lossy(xml);
    let Some(start) = text.find("Type=\"SkillsState\"") else {
        return (None, None);
    };
    let object = &text[start..];
    let object = &object[..object.find("</Object>").unwrap_or(object.len())];
    let property = |name: &str| -> Option<i64> {
        let at = object.find(&format!("Name=\"{name}\""))?;
        let rest = &object[at..];
        let rest = &rest[rest.find("Value=\"")? + 7..];
        rest[..rest.find('"')?].trim().parse().ok()
    };
    (
        property("SkillPointsTotal"),
        property("UnspentSkillPointsTotal"),
    )
}

/// A bundle that passed every check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidBundle {
    /// The label (Azimuth folder name as a number).
    pub label: u64,
    /// Its files, unpacked, in [`SAVE_FILES`] order.
    pub files: Vec<(&'static str, Vec<u8>)>,
    /// Its content digest.
    pub content: Digest,
    /// Its progress numbers.
    pub summary: SaveSummary,
}

impl ValidBundle {
    /// The label as Azimuth names the folder: 16 upper-case hex digits.
    pub fn label_hex(&self) -> String {
        format!("{:016X}", self.label)
    }
}

/// Shape, unpacking and content checks, then digest and summary.
pub fn validate(data: &[u8]) -> Result<ValidBundle, BundleError> {
    let layout = parse_layout(data)?;
    let files = unpack(&layout)?;
    check_contents(&files)?;
    let content = content_digest(&files);
    let summary = summary(&files);
    Ok(ValidBundle {
        label: layout.label,
        files,
        content,
        summary,
    })
}

/// What the server keeps of a bundle that passed [`verify`]: no file bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The label (Azimuth folder name as a number).
    pub label: u64,
    /// Its content digest.
    pub content: Digest,
    /// Its progress numbers.
    pub summary: SaveSummary,
}

impl Verified {
    /// The label as Azimuth names the folder: 16 upper-case hex digits.
    pub fn label_hex(&self) -> String {
        format!("{:016X}", self.label)
    }
}

fn decode(entry: &Entry<'_>) -> Result<Vec<u8>, BundleError> {
    let size = entry.raw_size as usize;
    match entry.method {
        0 => Some(entry.stored.to_vec()),
        1 => xpress_huffman_decode(entry.stored, size),
        2 => lz_decode(entry.stored, size),
        _ => None,
    }
    .filter(|b| b.len() == size)
    .ok_or(BundleError::Unpack(entry.name))
}

/// [`validate`] for the server's upload path: the same checks, refusals and results, in one pass
/// that unpacks one file at a time, hashes it once (Beta and [`validate`] hash every file twice:
/// for its `commit` line and for the content digest) and drops it before the next. Peak memory is
/// the largest file (PlayerDatabase, 6 MB) instead of the whole unpacked save, and nothing is kept
/// once it returns. For a bundle with several faults it reports the one [`validate`] reports.
pub fn verify(data: &[u8]) -> Result<Verified, BundleError> {
    let layout = parse_layout(data)?;
    let entry = |name: &str| layout.entries.iter().find(|e| e.name == name);
    // `commit` is required (parse_layout) and first in canonical order.
    let commit_entry = entry("commit").ok_or(BundleError::MissingFile("commit"))?;
    let commit = String::from_utf8_lossy(&decode(commit_entry)?).into_owned();
    let mut digests = Vec::with_capacity(SAVE_FILES.len() * 64);
    // validate() unpacks every file before any content check, so an unpack failure anywhere wins
    // over a content failure; the first content failure is kept and reported after the loop.
    let mut mismatch: Option<&'static str> = None;
    let (mut database_ok, mut xml_ok) = (false, false);
    let mut summary = SaveSummary::default();
    let mut skills = (None, None);
    for name in SAVE_FILES.iter().skip(1).copied() {
        let Some(e) = entry(name) else { continue };
        let body = decode(e)?;
        let sha = Digest::of(&body);
        let short = name.trim_start_matches("data/");
        let line = format!("{} {} {short}", sha.hex(), body.len());
        if mismatch.is_none() && !commit.lines().any(|l| l.trim_end_matches('\r') == line) {
            mismatch = Some(name);
        }
        match name {
            "data/PlayerDatabase" => {
                let inner = body
                    .get(4..8)
                    .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
                    .unwrap_or(0);
                database_ok = body.get(..4) == Some(b"ssmc".as_slice())
                    && inner + 8 <= body.len()
                    && body.get(8..24) == Some(b"SQLite format 3\0".as_slice());
            }
            "data/SaveState" => {
                xml_ok = body
                    .iter()
                    .find(|b| !(b.is_ascii_whitespace() || matches!(**b, 0xef | 0xbb | 0xbf)))
                    == Some(&b'<');
                skills = save_state_skills(&body);
            }
            "data/ProfileData" => {
                summary = profile_summary(&body);
            }
            _ => {}
        }
        digests.extend_from_slice(name.as_bytes());
        digests.push(0);
        digests.extend_from_slice(&sha.0);
    }
    if let Some(name) = mismatch {
        return Err(BundleError::CommitMismatch(name));
    }
    if !database_ok {
        return Err(BundleError::NotGameDatabase);
    }
    if !xml_ok {
        return Err(BundleError::NotXml);
    }
    (summary.skill_points, summary.unspent_skill_points) = skills;
    Ok(Verified {
        label: layout.label,
        content: Digest::of(&digests),
        summary,
    })
}

// ------------------------------------------------------------------ the hook's LZ (method 2)

fn put_varint(out: &mut Vec<u8>, mut v: usize) {
    loop {
        let byte = (v & 127) as u8;
        v >>= 7;
        out.push(byte | if v != 0 { 128 } else { 0 });
        if v == 0 {
            break;
        }
    }
}

fn read32(p: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]])
}

fn lz_hash(p: &[u8], i: usize) -> usize {
    (read32(p, i).wrapping_mul(2_654_435_761) >> (32 - LZ_HASH_BITS)) as usize
}

/// `hook/adapters/azimuth_codec.c` `codec_lz_encode`: varint literal count, the literals, then
/// varint length - 4 and varint offset - 1; no empty run after a final match. Deterministic, so
/// packing the same files twice gives the same bytes.
pub fn lz_encode(input: &[u8]) -> Vec<u8> {
    let size = input.len();
    let mut out = Vec::with_capacity(size / 8 + 16);
    let mut table = vec![0u32; 1 << LZ_HASH_BITS];
    let (mut anchor, mut i) = (0usize, 0usize);
    let literals = |out: &mut Vec<u8>, from: usize, to: usize| {
        put_varint(out, to - from);
        out.extend_from_slice(&input[from..to]);
    };
    while i + LZ_MIN_MATCH <= size {
        let h = lz_hash(input, i);
        let candidate = table[h] as usize;
        table[h] = (i + 1) as u32;
        if candidate == 0 || read32(input, candidate - 1) != read32(input, i) {
            i += 1;
            continue;
        }
        let from = candidate - 1;
        let mut length = LZ_MIN_MATCH;
        while i + length < size && input[from + length] == input[i + length] {
            length += 1;
        }
        literals(&mut out, anchor, i);
        put_varint(&mut out, length - LZ_MIN_MATCH);
        put_varint(&mut out, i - from - 1);
        let end = i + length;
        let mut p = i + 1;
        while p + LZ_MIN_MATCH <= size && p < end && p < i + 16 {
            table[lz_hash(input, p)] = (p + 1) as u32;
            p += 1;
        }
        if end - 1 > i + 16 && end - 1 + LZ_MIN_MATCH <= size {
            table[lz_hash(input, end - 1)] = end as u32;
        }
        i = end;
        anchor = end;
    }
    if anchor < size {
        literals(&mut out, anchor, size);
    }
    out
}

fn varint(input: &[u8], at: &mut usize) -> Option<usize> {
    let mut v = 0usize;
    for shift in (0..35).step_by(7) {
        let b = *input.get(*at)?;
        *at += 1;
        v |= ((b & 127) as usize) << shift;
        if b & 128 == 0 {
            return Some(v);
        }
    }
    None
}

/// Decodes method 2; `None` unless the input decodes to exactly `size` bytes with nothing left.
pub fn lz_decode(input: &[u8], size: usize) -> Option<Vec<u8>> {
    let mut at = 0usize;
    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let n = varint(input, &mut at)?;
        if n > size - out.len() {
            return None;
        }
        out.extend_from_slice(input.get(at..at + n)?);
        at += n;
        if out.len() == size {
            break;
        }
        let length = varint(input, &mut at)? + 4;
        let offset = varint(input, &mut at)? + 1;
        if offset > out.len() || length > size - out.len() {
            return None;
        }
        for _ in 0..length {
            out.push(out[out.len() - offset]);
        }
    }
    (at == input.len()).then_some(out)
}

/// XPRESS Huffman (MS-XCA 2.2.4), what hooks before method 2 sent from Windows: blocks of 64 KiB
/// of output, each starting with 256 bytes of 4-bit code lengths for 512 symbols. Decode only;
/// nothing writes method 1 any more, but Beta saves imported by the migration may hold it.
pub fn xpress_huffman_decode(input: &[u8], size: usize) -> Option<Vec<u8>> {
    let word = |at: &mut usize| -> u32 {
        let v = u32::from(input.get(*at).copied().unwrap_or(0))
            | u32::from(input.get(*at + 1).copied().unwrap_or(0)) << 8;
        *at += 2;
        v
    };
    let take = |n: u8, bits: &mut u32, extra: &mut i32, at: &mut usize| {
        *bits = bits.wrapping_shl(u32::from(n));
        *extra -= i32::from(n);
        if *extra < 0 {
            *bits |= word(at) << (-*extra) as u32;
            *extra += 16;
        }
    };
    let mut out = Vec::with_capacity(size);
    let mut base = 0usize;
    let mut block_end = 0usize;
    while out.len() < size {
        let lengths = input.get(base..base + 256)?;
        let mut table = vec![(0u16, 0u8); 1 << 15];
        let mut code = 0usize;
        for length in 1..=15u8 {
            for symbol in 0..512usize {
                let l = if symbol % 2 == 0 {
                    lengths[symbol / 2] & 15
                } else {
                    lengths[symbol / 2] >> 4
                };
                if l == length {
                    let first = code << (15 - length);
                    let n = 1usize << (15 - length);
                    table
                        .get_mut(first..first + n)?
                        .fill((symbol as u16, length));
                    code += 1;
                }
            }
            code <<= 1;
        }
        let mut at = base + 256;
        let mut bits = word(&mut at) << 16 | word(&mut at);
        let mut extra = 16i32;
        block_end += 65_536;
        let end = block_end.min(size);
        while out.len() < end {
            let (symbol, length) = table[(bits >> 17) as usize];
            if length == 0 {
                return None;
            }
            take(length, &mut bits, &mut extra, &mut at);
            if symbol < 256 {
                out.push(symbol as u8);
                continue;
            }
            let symbol = symbol - 256;
            let mut length = (symbol & 15) as usize;
            let offset_bits = (symbol >> 4) as u8;
            if length == 15 {
                length = *input.get(at)? as usize + 15;
                at += 1;
                if length == 270 {
                    let b = input.get(at..at + 2)?;
                    length = u16::from_le_bytes([b[0], b[1]]) as usize;
                    at += 2;
                    if length == 0 {
                        let b = input.get(at..at + 4)?;
                        length = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
                        at += 4;
                    }
                }
            }
            length += 3;
            let offset = if offset_bits > 0 {
                ((bits >> (32 - u32::from(offset_bits))) | (1 << offset_bits)) as usize
            } else {
                1
            };
            if offset_bits > 0 {
                take(offset_bits, &mut bits, &mut extra, &mut at);
            }
            if offset > out.len() || length > size.saturating_sub(out.len()) + 65_536 {
                return None;
            }
            for _ in 0..length {
                out.push(out[out.len() - offset]);
            }
        }
        base = at;
    }
    out.truncate(size);
    Some(out)
}

/// Packs files into an FH2SAVE1 bundle: method 2 where it is smaller, else stored. Files are
/// written in the order given; pass them in [`SAVE_FILES`] order for canonical bundles.
pub fn pack(label: u64, files: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&label.to_le_bytes());
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for (name, data) in files {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        let packed = if data.is_empty() {
            Vec::new()
        } else {
            lz_encode(data)
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz_round_trips() {
        let mut data = Vec::new();
        for i in 0..50_000u32 {
            data.extend_from_slice(&(i % 977).to_le_bytes());
            if i % 13 == 0 {
                data.extend_from_slice(b"ForzaInfinity");
            }
        }
        data.extend(std::iter::repeat_n(0u8, 100_000));
        let packed = lz_encode(&data);
        assert!(packed.len() < data.len() / 4);
        assert_eq!(lz_decode(&packed, data.len()).unwrap(), data);
        assert_eq!(
            lz_decode(&packed, data.len() + 1),
            None,
            "wrong declared size"
        );
        let mut trailing = packed.clone();
        trailing.push(0);
        assert_eq!(lz_decode(&trailing, data.len()), None, "trailing input");
        assert_eq!(lz_decode(&lz_encode(b"abc"), 3).unwrap(), b"abc");
    }

    fn xpress_lengths(pairs: &[(usize, u8)]) -> Vec<u8> {
        let mut lengths = vec![0u8; 256];
        for &(symbol, len) in pairs {
            if symbol % 2 == 0 {
                lengths[symbol / 2] |= len;
            } else {
                lengths[symbol / 2] |= len << 4;
            }
        }
        lengths
    }

    #[test]
    fn xpress_literals_decode() {
        // Every byte value with an 8-bit code: canonical codes equal the byte values, so the bit
        // stream "ABCD" is the 16-bit little-endian words 0x4142 and 0x4344.
        let pairs: Vec<(usize, u8)> = (0..256).map(|s| (s, 8)).collect();
        let mut input = xpress_lengths(&pairs);
        input.extend_from_slice(&[0x42, 0x41, 0x44, 0x43]);
        assert_eq!(xpress_huffman_decode(&input, 4).unwrap(), b"ABCD");
    }

    #[test]
    fn xpress_matches_decode() {
        // 'A' = code 0 and match symbol 256 (length 3, offset 1) = code 1, both 1 bit long:
        // bits "01" decode to 'A' then a 3-byte copy of the previous byte.
        let mut input = xpress_lengths(&[(65, 1), (256, 1)]);
        input.extend_from_slice(&[0x00, 0x40]);
        assert_eq!(xpress_huffman_decode(&input, 4).unwrap(), b"AAAA");
        assert_eq!(
            xpress_huffman_decode(&input[..100], 4),
            None,
            "truncated length table"
        );
    }

    #[test]
    fn profile_markers_are_exact() {
        let mut profile = vec![0u8; 64];
        let mut put = |name: &str, value: i32| {
            profile.extend_from_slice(&profile_marker(name));
            profile.extend_from_slice(&value.to_le_bytes());
        };
        put("WristbandLevel", 7);
        put("Level", 42);
        put("XP", 123_456);
        assert_eq!(
            profile_int(&profile, "Level"),
            Some(42),
            "the length prefix keeps WristbandLevel apart"
        );
        assert_eq!(profile_int(&profile, "XP"), Some(123_456));
        assert_eq!(profile_int(&profile, "Credits"), None);
    }
}
