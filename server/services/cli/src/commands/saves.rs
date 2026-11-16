//! Save commands (OD-60, SAVES_API.md §4.8):
//!
//! - `seed-import --game <game> --kind hardcore (<bundle> | <revision folder> | --synthetic)`:
//!   packs a revision folder as the canonical bundle, then verifies the bundle
//!   (`fi_saves::bundle::verify`: the format, every file against its `commit` line, and readable
//!   credits, XP and level, which become the gates' base), puts it create-only in the save bucket
//!   under its digest (`CLOUD_SAVES_BUCKET`; `STORAGE_EMULATOR_HOST` locally), then records it as
//!   the game's active seed as the migrator (`DATABASE_MIGRATOR_URL`; `fi_db::seeds::import_in`),
//!   retiring the previous one. The active seed again changes nothing. `--synthetic` imports
//!   `fi_saves::synthetic::starter` (made-up bytes: only where `APP_ENV` or `FI_ENV` says `local`
//!   or `test`, never by the mere absence of `prod`). Nothing of the bundle is logged but its
//!   digest, label and numbers.

use super::Outcome;
use fi_db::seeds::{self, Imported, NewSeed};
use fi_db::{DbConfig, Workload};
use fi_saves::{BlobRef, BlobStore, Digest, bundle};
use fi_storage::Blobs;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  seed-import      --game fh2 --kind hardcore <bundle | revision folder> | --synthetic: verify a Hardcore seed
                   bundle, put it in the save bucket and record it as the game's active seed
                   (DATABASE_MIGRATOR_URL, CLOUD_SAVES_BUCKET, STORAGE_EMULATOR_HOST locally;
                   --synthetic: the synthetic starter, only with APP_ENV or FI_ENV local or test)";

/// Runs `command` with `args`; `None` when it is not a save command.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    Some(match command {
        "seed-import" => seed_import(args).await,
        _ => return None,
    })
}

/// The arguments of `seed-import`.
#[derive(Debug, PartialEq, Eq)]
struct Args {
    game: String,
    kind: String,
    /// `None`: `--synthetic`.
    path: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let (mut game, mut kind, mut path, mut synthetic) = (None, None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game" => game = it.next().cloned(),
            "--kind" => kind = it.next().cloned(),
            "--synthetic" => synthetic = true,
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => {
                if path.replace(other.to_owned()).is_some() {
                    return Err("one bundle at a time".into());
                }
            }
        }
    }
    let game = game.ok_or("--game is required")?;
    if !(2..=16).contains(&game.len()) || !game.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("--game is a game code, e.g. fh2".into());
    }
    let kind = kind.ok_or("--kind is required")?;
    if kind != seeds::HARDCORE {
        return Err("--kind is hardcore".into());
    }
    match (path, synthetic) {
        (Some(_), true) | (None, false) => Err("a bundle path or --synthetic, not both".into()),
        (path, _) => Ok(Args { game, kind, path }),
    }
}

/// A revision folder as Azimuth writes it (`<16 hex digits>/commit`, `data/...`), packed as the
/// canonical FH2SAVE1 bundle the launcher would upload: the files of [`bundle::SAVE_FILES`] that
/// exist, in that order, labelled with the folder's name read as hex. What WINDOWS captures from a
/// new profile (OD-60) is imported this way, without a hand-made bundle.
fn pack_revision_dir(path: &str) -> Result<Vec<u8>, String> {
    let dir = std::path::Path::new(path);
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{path}: not a revision folder"))?;
    if name.len() != 16 || !name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{path}: a revision folder is named with 16 hex digits"
        ));
    }
    let label = u64::from_str_radix(name, 16).map_err(|e| format!("{path}: {e}"))?;
    let mut files = Vec::new();
    for file in bundle::SAVE_FILES {
        let at = dir.join(file);
        if at.is_file() {
            files.push((
                file,
                std::fs::read(&at).map_err(|e| format!("{}: {e}", at.display()))?,
            ));
        }
    }
    let bytes = bundle::pack(label, &files);
    if bytes.len() > bundle::MAX_BUNDLE {
        return Err(format!(
            "{path}: a bundle is at most {} bytes",
            bundle::MAX_BUNDLE
        ));
    }
    Ok(bytes)
}

async fn seed_import(args: &[String]) -> Outcome {
    let args = parse(args)?;
    let bytes = match &args.path {
        Some(path) if std::path::Path::new(path).is_dir() => pack_revision_dir(path)?,
        Some(path) => {
            let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            if bytes.len() > bundle::MAX_BUNDLE {
                return Err(
                    format!("{path}: a bundle is at most {} bytes", bundle::MAX_BUNDLE).into(),
                );
            }
            bytes
        }
        None => {
            if !local_or_test() {
                return Err("--synthetic needs APP_ENV or FI_ENV set to local or test".into());
            }
            fi_saves::synthetic::starter().to_vec()
        }
    };
    let verified =
        bundle::verify(&bytes).map_err(|e| format!("the bundle does not verify: {e}"))?;
    let blob = BlobRef {
        sha256: Digest::of(&bytes),
        size: bytes.len() as u32,
        format: fi_saves::SaveFormat::of_magic(&bytes),
    };
    let store = Blobs::from_env()?;
    store.put(blob.sha256, &bytes).await?;
    let db = DbConfig::from_env_for("DATABASE_MIGRATOR_URL", "fi-cli-seed-import", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let mut tx = pool.begin().await?;
    let label = verified.label_hex();
    let imported = seeds::import_in(
        &mut tx,
        &NewSeed {
            game: &args.game,
            kind: &args.kind,
            blob,
            content: verified.content,
            label: &label,
            // The driver level its XP reaches, never ProfileData `Main/Level` (2026-10-31).
            summary: if args.game == "fh2" {
                fi_saves::gates::with_driver_level(&fi_gamedb::RULES, verified.summary)
            } else {
                verified.summary
            },
        },
    )
    .await?;
    tx.commit().await?;
    // Put again after the registration (create-only, idempotent): a garbage collection may have
    // deleted the object between the first put and the registration that cleared its mark
    // (CLOUD_SAVES.md §6 step 4).
    store.put(blob.sha256, &bytes).await?;
    let (what, seed) = match &imported {
        Imported::Created(s) => ("imported as the active seed", s),
        Imported::AlreadyActive(s) => ("already the active seed: nothing changed", s),
    };
    tracing::info!(
        game = %seed.game,
        kind = %seed.kind,
        blob = %seed.blob.sha256.hex(),
        label = %seed.label,
        credits = seed.summary.credits,
        xp = seed.summary.xp,
        level = seed.summary.level,
        cars = seed.summary.cars,
        synthetic = args.path.is_none(),
        "{what}"
    );
    println!(
        "seed-import: {what} ({} {}, blob {}, label {})",
        seed.game,
        seed.kind,
        seed.blob.sha256.hex(),
        seed.label
    );
    Ok(())
}

/// Whether `APP_ENV` or `FI_ENV` names a local environment or a test run, explicitly.
fn local_or_test() -> bool {
    ["APP_ENV", "FI_ENV"].into_iter().any(|n| {
        std::env::var(n)
            .ok()
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "local" | "test"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_revision_folder_packs_as_the_canonical_bundle_of_its_files() {
        let starter = fi_saves::synthetic::starter().to_vec();
        let files =
            bundle::unpack(&bundle::parse_layout(&starter).unwrap()).expect("synthetic unpacks");
        let root = std::env::temp_dir().join(format!("fi-cli-seed-{}", std::process::id()));
        let rev = root.join("0000000000000001");
        for (name, data) in &files {
            let at = rev.join(name);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, data).unwrap();
        }
        let packed = pack_revision_dir(rev.to_str().unwrap()).unwrap();
        let (ours, theirs) = (
            bundle::verify(&packed).unwrap(),
            bundle::verify(&starter).unwrap(),
        );
        assert_eq!(ours.label, 1);
        assert_eq!(
            ours.content, theirs.content,
            "the same files, whatever the encoding"
        );
        assert!(
            pack_revision_dir(root.to_str().unwrap()).is_err(),
            "not a revision name"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_arguments_name_a_game_the_kind_and_one_source() {
        assert_eq!(
            parse(&args(&[
                "--game",
                "fh2",
                "--kind",
                "hardcore",
                "seed.fh2save1"
            ])),
            Ok(Args {
                game: "fh2".into(),
                kind: "hardcore".into(),
                path: Some("seed.fh2save1".into())
            })
        );
        assert_eq!(
            parse(&args(&[
                "--synthetic",
                "--kind",
                "hardcore",
                "--game",
                "fh2"
            ]))
            .map(|a| a.path),
            Ok(None)
        );
        for bad in [
            &["--kind", "hardcore", "x"][..],
            &["--game", "fh2", "x"],
            &["--game", "fh2", "--kind", "noob", "x"],
            &["--game", "fh2", "--kind", "hardcore"],
            &["--game", "fh2", "--kind", "hardcore", "x", "--synthetic"],
            &["--game", "fh2", "--kind", "hardcore", "x", "y"],
            &["--game", "f h", "--kind", "hardcore", "x"],
            &["--game", "fh2", "--kind", "hardcore", "--force", "x"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_synthetic_starter_verifies_with_readable_numbers() {
        let v = bundle::verify(fi_saves::synthetic::starter()).unwrap();
        assert!(v.summary.credits.is_some() && v.summary.xp.is_some() && v.summary.level.is_some());
    }
}
