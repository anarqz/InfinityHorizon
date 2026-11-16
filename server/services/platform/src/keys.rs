//! `fi-platform keys init` (ADR 0023 §9, OD-122 M2): the secrets of a standalone instance,
//! generated once into its keys directory, one file per variable (`<dir>/<VARIABLE>`, mode 0600),
//! which the processes read through `FI_KEYS_DIR` when the variable is not set
//! ([`fi_surfaces::config::KeysDirEnv`], `fi_runtime::config::secret`, fi-db's URL composition).
//!
//! - Never overwrites: each file is written to a temporary name and hard-linked into place, which
//!   fails when the name exists, so a second run (every `docker compose up`) keeps every key and only
//!   adds a file a newer release introduced.
//! - Every value comes from the OS CSPRNG ([`fi_auth::random_bytes`]); rings carry a random kid
//!   (never a well-known dev label) and parse with the same functions the services use.
//! - Nothing here prints or logs a value: the report names files only.

use fi_auth::keys::SigningKeys;
use fi_auth::relay_grant::PublicKeys;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The Ed25519 rings: (variable, kid prefix).
const RINGS: [(&str, &str); 4] = [
    // Locally one ring signs launcher access tokens and game tickets (compose/infinity.yml); the
    // game surfaces get its public half in process.
    ("FI_TOKEN_SIGNING_KEYS", "at"),
    ("FI_LAUNCH_TICKET_SIGNING_KEYS", "lt"),
    // The `local` sign-in's ID tokens (never the access or ticket ring).
    ("FI_LOCAL_IDP_SIGNING_KEYS", "idp"),
    // Relay grants (fi-matchmaking signs, the relay verifies the public half).
    ("FI_RELAY_GRANT_SIGNING_KEYS", "rg"),
];

/// Database passwords (hex, so they need no URL encoding): the Postgres superuser (initdb only),
/// the migrator login (initdb creates it) and the two runtime logins (`migrate` creates them).
const PASSWORDS: [&str; 4] = [
    "POSTGRES_PASSWORD",
    "FI_MIGRATOR_PASSWORD",
    "FI_APP_PASSWORD",
    "FI_JOBS_PASSWORD",
];

/// The server's own identity in the beholder's directory (ANNOUNCER_API.md §1): base64 of an
/// Ed25519 seed, read only when `FI_BEHOLDER_URL` is set ([`crate::announce`]).
pub const ANNOUNCE_KEY: &str = "FI_ANNOUNCE_SIGNING_KEY";

/// Every file `keys init` writes (the relay's public ring after the others: it is derived).
pub fn names() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = RINGS.iter().map(|(n, _)| *n).collect();
    all.push("FI_SITE_KEY_PEPPER");
    all.extend(PASSWORDS);
    all.push("FI_RELAY_GRANT_VERIFY_KEYS");
    all.push(ANNOUNCE_KEY);
    all
}

/// What a run did, by file name.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Files created by this run.
    pub written: Vec<&'static str>,
    /// Files that existed and were left as they are.
    pub kept: Vec<&'static str>,
}

/// A failure, naming a file but never a value.
#[derive(Debug, thiserror::Error)]
pub enum KeysError {
    /// The OS random source failed.
    #[error("the system random source is unavailable")]
    Random,
    /// A file could not be written or read.
    #[error("{name}: {source}")]
    Io {
        /// The file.
        name: String,
        /// Why.
        source: std::io::Error,
    },
    /// An existing file does not parse (fix or remove it by hand).
    #[error("{0} exists but does not parse")]
    Invalid(&'static str),
}

fn hex_random<const N: usize>() -> Result<String, KeysError> {
    Ok(fi_auth::hex(
        &fi_auth::random_bytes::<N>().map_err(|_| KeysError::Random)?,
    ))
}

fn ring(prefix: &str) -> Result<String, KeysError> {
    let seed = fi_auth::random_bytes::<32>().map_err(|_| KeysError::Random)?;
    Ok(format!(
        "{prefix}-{}:{}",
        hex_random::<4>()?,
        fi_auth::b64::std_encode(&seed)
    ))
}

fn pepper() -> Result<String, KeysError> {
    let secret = fi_auth::random_bytes::<32>().map_err(|_| KeysError::Random)?;
    Ok(format!("1:{}", fi_auth::b64::std_encode(&secret)))
}

fn io(name: &str) -> impl FnOnce(std::io::Error) -> KeysError + '_ {
    move |source| KeysError::Io {
        name: name.to_owned(),
        source,
    }
}

/// Creates `dir/name` holding `value` unless it exists (`false` then). Written to a temporary file
/// (0600) first and hard-linked into place, so a reader never sees half a key and an existing file
/// is never replaced.
fn create_once(dir: &Path, name: &'static str, value: &str) -> Result<bool, KeysError> {
    let target = dir.join(name);
    if target.exists() {
        return Ok(false);
    }
    let partial: PathBuf = dir.join(format!(".{name}.{}.partial", hex_random::<6>()?));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&partial)?;
        file.write_all(value.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::hard_link(&partial, &target)
    })();
    let _ = std::fs::remove_file(&partial);
    match result {
        Ok(()) => Ok(true),
        // Another run created it meanwhile: theirs stays.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(io(name)(e)),
    }
}

fn read(dir: &Path, name: &'static str) -> Result<String, KeysError> {
    fi_runtime::config::key_file(dir, name).ok_or(KeysError::Invalid(name))
}

/// Writes every missing secret into `dir` (created 0700 when absent) and returns what it did.
pub fn init(dir: &Path) -> Result<Report, KeysError> {
    if !dir.exists() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(dir).map_err(io("the keys directory"))?;
    }
    // A volume mount point arrives 0755 (the image cannot carry the mode): only the owner lists
    // the secrets' names (review L1, 2026-10-25).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(io("the keys directory"))?;
    }
    let mut report = Report::default();
    let mut record = |name, written: bool| {
        if written {
            report.written.push(name);
        } else {
            report.kept.push(name);
        }
    };
    for (name, prefix) in RINGS {
        record(name, create_once(dir, name, &ring(prefix)?)?);
        // An existing ring must still be one the services accept.
        SigningKeys::parse(name, &read(dir, name)?).map_err(|_| KeysError::Invalid(name))?;
    }
    record(
        "FI_SITE_KEY_PEPPER",
        create_once(dir, "FI_SITE_KEY_PEPPER", &pepper()?)?,
    );
    fi_auth::keys::parse_pepper("FI_SITE_KEY_PEPPER", &read(dir, "FI_SITE_KEY_PEPPER")?)
        .map_err(|_| KeysError::Invalid("FI_SITE_KEY_PEPPER"))?;
    for name in PASSWORDS {
        record(name, create_once(dir, name, &hex_random::<32>()?)?);
    }
    // The relay's public ring, from the signing ring on disk (kept or just written).
    let relay = SigningKeys::parse(
        "FI_RELAY_GRANT_SIGNING_KEYS",
        &read(dir, "FI_RELAY_GRANT_SIGNING_KEYS")?,
    )
    .map_err(|_| KeysError::Invalid("FI_RELAY_GRANT_SIGNING_KEYS"))?;
    record(
        "FI_RELAY_GRANT_VERIFY_KEYS",
        create_once(
            dir,
            "FI_RELAY_GRANT_VERIFY_KEYS",
            &PublicKeys::of(&relay).to_text(),
        )?,
    );
    let announce = fi_announce::NodeKey::generate().map_err(|_| KeysError::Random)?;
    record(
        ANNOUNCE_KEY,
        create_once(dir, ANNOUNCE_KEY, &announce.to_text())?,
    );
    fi_announce::NodeKey::parse(&read(dir, ANNOUNCE_KEY)?)
        .map_err(|_| KeysError::Invalid(ANNOUNCE_KEY))?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fi-platform-keys-{tag}-{}-{}",
            std::process::id(),
            hex_random::<4>().unwrap()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn init_writes_every_secret_once_and_never_overwrites() {
        let dir = scratch("once");
        let first = init(&dir).unwrap();
        assert_eq!(first.written, names());
        assert!(first.kept.is_empty());
        let before: Vec<String> = names().iter().map(|n| read(&dir, n).unwrap()).collect();
        let second = init(&dir).unwrap();
        assert!(second.written.is_empty());
        assert_eq!(second.kept, names());
        let after: Vec<String> = names().iter().map(|n| read(&dir, n).unwrap()).collect();
        assert_eq!(before, after, "a second run changes nothing");
        // A file a newer release adds is created beside the kept ones.
        std::fs::remove_file(dir.join("FI_RELAY_GRANT_VERIFY_KEYS")).unwrap();
        let third = init(&dir).unwrap();
        assert_eq!(third.written, vec!["FI_RELAY_GRANT_VERIFY_KEYS"]);
        let at = |name| names().iter().position(|n| *n == name).unwrap();
        assert_eq!(
            read(&dir, "FI_RELAY_GRANT_VERIFY_KEYS").unwrap(),
            after[at("FI_RELAY_GRANT_VERIFY_KEYS")]
        );
        // The announce key is the node's identity: kept like every other key.
        assert_eq!(read(&dir, ANNOUNCE_KEY).unwrap(), after[at(ANNOUNCE_KEY)]);
        fi_announce::NodeKey::parse(&after[at(ANNOUNCE_KEY)]).unwrap();
        // No temporary file is left behind.
        let stray = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')
            })
            .count();
        assert_eq!(stray, 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn generated_values_are_what_the_services_accept_and_never_the_dev_ones() {
        let dir = scratch("formats");
        init(&dir).unwrap();
        let mut seeds = Vec::new();
        for (name, prefix) in RINGS {
            let text = read(&dir, name).unwrap();
            let ring = SigningKeys::parse(name, &text).unwrap();
            assert!(!ring.has_local_dev_key(), "{name}");
            let (kid, key) = ring.current();
            assert!(kid.starts_with(&format!("{prefix}-")) && !kid.contains("local"));
            seeds.push(key.to_bytes());
        }
        seeds.sort();
        seeds.dedup();
        assert_eq!(seeds.len(), RINGS.len(), "every ring has its own key");
        let pepper =
            fi_auth::keys::parse_pepper("P", &read(&dir, "FI_SITE_KEY_PEPPER").unwrap()).unwrap();
        assert!(
            !pepper
                .secrets()
                .any(|s| fi_auth::keys::is_local_dev_secret(s))
        );
        let verify = read(&dir, "FI_RELAY_GRANT_VERIFY_KEYS").unwrap();
        let signing =
            SigningKeys::parse("R", &read(&dir, "FI_RELAY_GRANT_SIGNING_KEYS").unwrap()).unwrap();
        assert_eq!(verify, PublicKeys::of(&signing).to_text());
        PublicKeys::parse("FI_RELAY_GRANT_VERIFY_KEYS", &verify).unwrap();
        for name in PASSWORDS {
            let pw = read(&dir, name).unwrap();
            assert_eq!(pw.len(), 64);
            assert!(pw.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in names() {
                let mode = std::fs::metadata(dir.join(name))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{name}");
            }
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_broken_existing_ring_is_reported_not_replaced() {
        let dir = scratch("broken");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("FI_TOKEN_SIGNING_KEYS"), "not a ring").unwrap();
        let e = init(&dir).unwrap_err();
        assert!(matches!(e, KeysError::Invalid("FI_TOKEN_SIGNING_KEYS")));
        assert_eq!(
            std::fs::read_to_string(dir.join("FI_TOKEN_SIGNING_KEYS")).unwrap(),
            "not a ring"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
