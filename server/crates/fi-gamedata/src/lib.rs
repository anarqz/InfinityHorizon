//! The operator's game data directory (T-1115, OD-247 Q3).
//!
//! The server compiles and ships with no game-derived file: every table read from the game is a
//! file of the directory [`ENV`] (`FI_GAME_DATA_DIR`) names, rebuilt from the operator's own game
//! install by `tools/gamedata` and listed with its SHA-256 in the directory's [`MANIFEST`]:
//!
//! ```text
//! <FI_GAME_DATA_DIR>/
//!   manifest.json             {"format": "fi-gamedata/1", "files": {"fh2/catalog.json": "<sha256>", ...}}
//!   fh2/xtsw-schema.json      the web-service schema (fi-xtsw)
//!   fh2/catalog.json          cars, classes, buckets, tracks, perks (fi-gamedb)
//!   fh2/...                   the other FH2 tables (tools/gamedata/README.md lists every file)
//!   fm5/xtsw-schema.json      Forza Motorsport 5's web-service schema (fi-xtsw; only when FI_GAMES has fm5)
//! ```
//!
//! A process opens the directory once ([`dir`]) and each table is parsed once, on first use, into a
//! [`Lazy`] the crate that owns the table holds. A process checks everything at start
//! (`fi_surfaces::gamedata::load`), so a missing setting, a missing or changed file, or a file that
//! does not parse stops it before it serves, with an [`Error`] naming the file; the [`Lazy`]
//! accessors then never fail. Nothing falls back: a table that cannot be read is an error, never an
//! empty one.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use sha2::Digest;

/// The setting naming the directory.
pub const ENV: &str = "FI_GAME_DATA_DIR";

/// The directory's manifest: every file with its SHA-256.
pub const MANIFEST: &str = "manifest.json";

/// The manifest's `format`.
pub const FORMAT: &str = "fi-gamedata/1";

/// Why the game data cannot be used: the file (or the setting) and the problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    file: String,
    problem: String,
}

impl Error {
    /// An error about `file` (a path, or [`ENV`]).
    pub fn new(file: impl Into<String>, problem: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            problem: problem.into(),
        }
    }

    /// The file (or setting) the error is about.
    pub fn file(&self) -> &str {
        &self.file
    }

    /// What is wrong with it.
    pub fn problem(&self) -> &str {
        &self.problem
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "game data: {}: {}", self.file, self.problem)
    }
}

impl std::error::Error for Error {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    format: String,
    files: BTreeMap<String, String>,
}

/// An opened game data directory: its root and its manifest.
#[derive(Debug)]
pub struct Dir {
    root: PathBuf,
    files: BTreeMap<String, [u8; 32]>,
}

impl Dir {
    /// Opens `root` and reads its [`MANIFEST`] (no data file is read yet).
    pub fn open(root: impl Into<PathBuf>) -> Result<Dir, Error> {
        let root = root.into();
        if !root.is_dir() {
            return Err(Error::new(
                root.display().to_string(),
                format!("is not a directory ({ENV})"),
            ));
        }
        let path = root.join(MANIFEST);
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Error::new(&shown, format!("cannot be read: {e}")))?;
        let raw: RawManifest = serde_json::from_str(&text)
            .map_err(|e| Error::new(&shown, format!("is not a manifest: {e}")))?;
        if raw.format != FORMAT {
            return Err(Error::new(
                &shown,
                format!("format {:?}, expected {FORMAT:?}", raw.format),
            ));
        }
        if raw.files.is_empty() {
            return Err(Error::new(&shown, "lists no file"));
        }
        let mut files = BTreeMap::new();
        for (name, hex) in raw.files {
            if !valid_name(&name) {
                return Err(Error::new(
                    &shown,
                    format!("{name:?} is not a relative path of plain names"),
                ));
            }
            let digest = unhex(&hex).ok_or_else(|| {
                Error::new(
                    &shown,
                    format!("{name:?}: {hex:?} is not a lowercase hex SHA-256"),
                )
            })?;
            files.insert(name, digest);
        }
        Ok(Dir { root, files })
    }

    /// The directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The files the manifest lists, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The bytes of `name`, which must be listed and match its SHA-256.
    pub fn read_bytes(&self, name: &str) -> Result<Vec<u8>, Error> {
        let path = self.root.join(name);
        let shown = path.display().to_string();
        let Some(want) = self.files.get(name) else {
            return Err(Error::new(shown, format!("is not listed in {MANIFEST}")));
        };
        let bytes =
            std::fs::read(&path).map_err(|e| Error::new(&shown, format!("cannot be read: {e}")))?;
        let got: [u8; 32] = sha2::Sha256::digest(&bytes).into();
        if got != *want {
            return Err(Error::new(
                shown,
                format!(
                    "SHA-256 {} differs from {MANIFEST}'s {}",
                    hex(&got),
                    hex(want)
                ),
            ));
        }
        Ok(bytes)
    }

    /// The text of `name` ([`Dir::read_bytes`], UTF-8).
    pub fn read(&self, name: &str) -> Result<String, Error> {
        let bytes = self.read_bytes(name)?;
        String::from_utf8(bytes)
            .map_err(|_| Error::new(self.root.join(name).display().to_string(), "is not UTF-8"))
    }

    /// Reads `name` and parses it with `parse`; a parse error names the file.
    pub fn parse<T, E: fmt::Display>(
        &self,
        name: &str,
        parse: impl FnOnce(&str) -> Result<T, E>,
    ) -> Result<T, Error> {
        let text = self.read(name)?;
        parse(&text).map_err(|e| {
            Error::new(
                self.root.join(name).display().to_string(),
                format!("is not valid: {e}"),
            )
        })
    }

    /// Checks every listed file against its SHA-256; the number of files.
    pub fn verify(&self) -> Result<usize, Error> {
        for name in self.files.keys() {
            self.read_bytes(name)?;
        }
        Ok(self.files.len())
    }
}

/// A manifest name: `/`-separated plain names (letters, digits, `.`, `_`, `-`), never `.` or `..`,
/// never the manifest itself.
fn valid_name(name: &str) -> bool {
    name != MANIFEST
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
        out.push(char::from(b"0123456789abcdef"[usize::from(b & 0xf)]));
    }
    out
}

/// The process's directory: [`ENV`], opened on first use (and kept, with its error).
pub fn dir() -> Result<&'static Dir, Error> {
    static DIR: OnceLock<Result<Dir, Error>> = OnceLock::new();
    DIR.get_or_init(|| match std::env::var_os(ENV) {
        Some(path) if !path.is_empty() => Dir::open(PathBuf::from(path)),
        _ => Err(Error::new(
            ENV,
            "is not set: the directory of the operator's game data (tools/gamedata/README.md)",
        )),
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// Reads `name` of the process's directory and parses it ([`Dir::parse`]).
pub fn parse<T, E: fmt::Display>(
    name: &str,
    parse: impl FnOnce(&str) -> Result<T, E>,
) -> Result<T, Error> {
    dir()?.parse(name, parse)
}

/// A table of the game data, loaded once on first use. [`Lazy::load`] answers the loader's error;
/// dereferencing panics with it, which a process that ran `fi_surfaces::gamedata::load` at start
/// never sees.
pub struct Lazy<T> {
    load: fn() -> Result<T, Error>,
    cell: OnceLock<Result<T, Error>>,
}

impl<T> Lazy<T> {
    /// A table `load` reads (usually with [`parse`]).
    pub const fn new(load: fn() -> Result<T, Error>) -> Self {
        Self {
            load,
            cell: OnceLock::new(),
        }
    }

    /// The table, loaded on the first call (named `load`, not `get`, so a table's own `get`, such
    /// as a `Vec`'s, stays reachable through [`Deref`]).
    pub fn load(&self) -> Result<&T, &Error> {
        self.cell.get_or_init(self.load).as_ref()
    }
}

impl<T> Deref for Lazy<T> {
    type Target = T;

    fn deref(&self) -> &T {
        match self.load() {
            Ok(value) => value,
            Err(e) => panic!("{e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_dir(files: &[(&str, &[u8])], manifest: Option<String>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut listed = BTreeMap::new();
        for (name, bytes) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, bytes).unwrap();
            listed.insert(
                (*name).to_owned(),
                hex(&<[u8; 32]>::from(sha2::Sha256::digest(bytes))),
            );
        }
        let manifest = manifest.unwrap_or_else(|| {
            serde_json::json!({ "format": FORMAT, "files": listed }).to_string()
        });
        std::fs::write(dir.path().join(MANIFEST), manifest).unwrap();
        dir
    }

    #[test]
    fn a_listed_file_reads_and_parses() {
        let tmp = write_dir(&[("fh2/a.json", b"{\"n\": 1}"), ("fm5/b.txt", b"x")], None);
        let dir = Dir::open(tmp.path()).unwrap();
        assert_eq!(dir.verify(), Ok(2));
        assert_eq!(dir.names().collect::<Vec<_>>(), ["fh2/a.json", "fm5/b.txt"]);
        let v: serde_json::Value = dir
            .parse("fh2/a.json", |t| serde_json::from_str(t))
            .unwrap();
        assert_eq!(v["n"], 1);
        assert_eq!(dir.read("fm5/b.txt").unwrap(), "x");
    }

    #[test]
    fn every_problem_names_its_file() {
        let tmp = write_dir(&[("fh2/a.json", b"{}")], None);
        let dir = Dir::open(tmp.path()).unwrap();
        // Not listed.
        let e = dir.read("fh2/other.json").unwrap_err();
        assert!(e.file().ends_with("fh2/other.json"), "{e}");
        assert!(e.problem().contains("not listed"), "{e}");
        // Malformed.
        let e = dir
            .parse("fh2/a.json", |t| serde_json::from_str::<Vec<i64>>(t))
            .unwrap_err();
        assert!(e.file().ends_with("fh2/a.json") && e.problem().contains("not valid"));
        // Changed after the manifest.
        std::fs::write(tmp.path().join("fh2/a.json"), b"{ }").unwrap();
        let e = dir.verify().unwrap_err();
        assert!(e.file().ends_with("fh2/a.json") && e.problem().contains("SHA-256"));
        assert!(e.to_string().starts_with("game data: "), "{e}");
        // Missing.
        std::fs::remove_file(tmp.path().join("fh2/a.json")).unwrap();
        let e = dir.read("fh2/a.json").unwrap_err();
        assert!(e.problem().contains("cannot be read"), "{e}");
    }

    #[test]
    fn a_bad_directory_or_manifest_is_refused() {
        let missing = tempfile::tempdir().unwrap().path().join("nothing");
        assert!(
            Dir::open(&missing)
                .unwrap_err()
                .problem()
                .contains("not a directory")
        );
        let empty = tempfile::tempdir().unwrap();
        let e = Dir::open(empty.path()).unwrap_err();
        assert!(e.file().ends_with(MANIFEST) && e.problem().contains("cannot be read"));
        for manifest in [
            "{".to_owned(),
            serde_json::json!({"format": "other/1", "files": {"a": hex(&[0; 32])}}).to_string(),
            serde_json::json!({"format": FORMAT, "files": {}}).to_string(),
            serde_json::json!({"format": FORMAT, "files": {"../a": hex(&[0; 32])}}).to_string(),
            serde_json::json!({"format": FORMAT, "files": {"/a": hex(&[0; 32])}}).to_string(),
            serde_json::json!({"format": FORMAT, "files": {"a": "ABC"}}).to_string(),
            serde_json::json!({"format": FORMAT, "files": {"a": hex(&[0; 32])}, "x": 1})
                .to_string(),
        ] {
            let tmp = write_dir(&[], Some(manifest.clone()));
            let e = Dir::open(tmp.path()).unwrap_err();
            assert!(e.file().ends_with(MANIFEST), "{manifest}: {e}");
        }
    }

    #[test]
    fn names_are_plain_relative_paths() {
        assert!(valid_name("fh2/catalog.json"));
        assert!(valid_name("fm5/xtsw-schema.json"));
        for bad in [
            "", "/a", "a/", "a//b", "./a", "a/../b", "a b", MANIFEST, "a\\b",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert_eq!(unhex(&hex(&[0xab; 32])), Some([0xab; 32]));
    }

    #[test]
    fn a_lazy_table_keeps_its_error() {
        static BAD: Lazy<i32> = Lazy::new(|| Err(Error::new("fh2/x.json", "is not valid: nope")));
        static GOOD: Lazy<i32> = Lazy::new(|| Ok(7));
        assert_eq!(BAD.load().unwrap_err().file(), "fh2/x.json");
        assert_eq!(*GOOD, 7);
        let panic = std::panic::catch_unwind(|| *BAD).unwrap_err();
        let text = panic.downcast_ref::<String>().unwrap();
        assert_eq!(text, "game data: fh2/x.json: is not valid: nope");
    }
}
