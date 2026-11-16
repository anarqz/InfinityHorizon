//! `dashboard-snapshot` (DASHBOARD_API.md §5 and §5.1, OD-173): the play dashboard's hourly
//! snapshot. The content is `fi_surfaces::dashboard::snapshot`'s (the live routes' reads and
//! assembly, the game's ranking, each person's opaque ref); this module reads it in one consistent
//! transaction, writes it as files and swaps it in:
//!
//! ```text
//! <dir>/snapshots/<yyyymmddThhmmssZ>/manifest.json
//!                                   /fh2/<mode>/boards.json
//!                                   /fh2/<mode>/boards/<family>/<board>.json
//!                                   /fh2/<mode>/players.json
//!                                   /fh2/<mode>/clubs.json
//!                                   /fh2/<mode>/clubs/<TAG>.json
//!                                   /fh2/<mode>/creations.json
//!                                   /fh2/profiles/<lower(name)>.json
//!                                   /fh2/profiles/<lower(name)>.garage.json
//! <dir>/current -> snapshots/<...>
//! ```
//!
//! **Consistent.** Every read runs in one `REPEATABLE READ READ ONLY` transaction: the snapshot is
//! one moment of the database (a profile renamed mid-run cannot appear twice or under two names).
//!
//! **Atomic.** Files go into `snapshots/<ts>.tmp`; before the swap every file and every folder of
//! the tree is fsynced, the folder is renamed to `snapshots/<ts>`, and `current` is replaced by
//! renaming a new relative symlink over it (the site mounts the folder at another path). Any
//! failure before that rename removes the temporary folder and leaves `current` as it was. After
//! the swap the run has succeeded: pruning keeps the newest [`SNAPSHOTS_KEPT`] (never the current
//! one) and removes temporary folders older than an hour (a concurrent run's recent one stays),
//! and a pruning failure is logged, never reported as the run's failure. Leftover `.current-*.tmp`
//! links of a crashed run are removed at the start.
//!
//! **Bounded.** Files are 0644 and folders 0755 (the site's container reads them as another
//! user); the [`SNAPSHOT_MAX_BYTES`] guard is checked file by file while the snapshot is built.
//! The log carries counts, bytes and the duration, never a name.

use super::Outcome;
use fi_db::{DbConfig, Workload};
use fi_domain::UnixMs;
use fi_protocol::dashboard::{
    SNAPSHOT_MAX_BYTES, SNAPSHOT_VERSION, SNAPSHOTS_KEPT, SnapshotGame, SnapshotManifest,
};
use fi_runtime::config;
use fi_surfaces::dashboard::RefKey;
use fi_surfaces::dashboard::snapshot::{Counts, Sink, Snapshotter};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  dashboard-snapshot  write the play dashboard's snapshot (DASHBOARD_API.md section 5) under
                   FI_DASHBOARD_SNAPSHOT_DIR (required) from DATABASE_URL, keyed by
                   FI_DASHBOARD_KEY (required, the profile refs): every board with its listed
                   rows, the players and their profiles, the clubs per mode, read in one consistent
                   transaction; swapped in atomically through the `current` symlink, the last 3 kept";

/// The games written (FH2 is the only one; its catalog drives the boards).
const GAMES: [&str; 1] = ["fh2"];
/// The modes written.
const MODES: [&str; 2] = ["hardcore", "noob"];
/// A temporary folder younger than this may be a running job's: pruning leaves it.
const STALE_TMP: Duration = Duration::from_secs(3_600);

/// Runs `command` with `args`; `None` when it is not this module's.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    match command {
        "dashboard-snapshot" if args.is_empty() => Some(dashboard_snapshot().await),
        "dashboard-snapshot" => Some(Err("dashboard-snapshot takes no arguments".into())),
        _ => None,
    }
}

/// RFC 3339 UTC of a Unix millisecond instant.
fn rfc3339(ms: i64) -> String {
    fi_protocol::instant::format_unix_micros(ms.saturating_mul(1_000)).unwrap_or_default()
}

/// The snapshot folder's name: `yyyymmddThhmmssZ` of the RFC 3339 instant.
pub fn folder_name(rfc3339: &str) -> String {
    let digits: String = rfc3339
        .chars()
        .take(19)
        .filter(|c| c.is_ascii_digit() || *c == 'T')
        .collect();
    format!("{digits}Z")
}

/// The manifest of a snapshot generated at `now_ms` every `interval` seconds.
pub fn manifest(
    now_ms: i64,
    interval: u64,
    games: BTreeMap<String, SnapshotGame>,
) -> SnapshotManifest {
    SnapshotManifest {
        version: SNAPSHOT_VERSION,
        generated_at: rfc3339(now_ms),
        next_at: rfc3339(now_ms.saturating_add((interval as i64).saturating_mul(1_000))),
        interval_secs: interval,
        games,
    }
}

async fn dashboard_snapshot() -> Outcome {
    let dir = config::optional("FI_DASHBOARD_SNAPSHOT_DIR").ok_or(
        "FI_DASHBOARD_SNAPSHOT_DIR is not set: the folder the snapshot is written to \
         (notebook: /data/dashboard, the host's ~/infinity-data/dashboard)",
    )?;
    let key = config::secret("FI_DASHBOARD_KEY").unwrap_or_default();
    let refs = match fi_surfaces::dashboard::checked_key(&key)? {
        Some(text) => RefKey::new(text),
        None => None,
    }
    .ok_or(
        "FI_DASHBOARD_KEY is not set: the snapshot's profile refs are keyed by the secret the \
         site shares (the keys volume or jobs.env)",
    )?;
    let started = std::time::Instant::now();
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-dashboard-snapshot", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let now = UnixMs::now_system().millis();
    let month = fi_surfaces::dashboard::boards::current_month();
    let interval = super::schedule::LANE.1;
    let mut staging = Staging::begin(
        Path::new(&dir),
        &folder_name(&rfc3339(now)),
        SNAPSHOT_MAX_BYTES,
    )?;
    let mut snap = Snapshotter::new(&refs, SNAPSHOT_MAX_BYTES);
    let mut games: BTreeMap<String, SnapshotGame> = BTreeMap::new();
    {
        let mut tx = pool.begin().await?;
        // The first statement of the transaction: one consistent moment, nothing written.
        sqlx_exec(
            &mut tx,
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY",
        )
        .await?;
        // Writing a mode's files happens between its reads; the job's 60 s idle cap is too tight
        // for a large mode on a slow disk, so this transaction alone gets ten minutes.
        sqlx_exec(
            &mut tx,
            "SET LOCAL idle_in_transaction_session_timeout = '10min'",
        )
        .await?;
        for game in GAMES {
            if !fi_db::dashboard::game_known_in(&mut tx, game).await? {
                return Err(format!("game {game} is not in this database").into());
            }
            for mode in MODES {
                snap.mode_in(&mut tx, game, mode, month, &mut staging)
                    .await?;
                games
                    .entry(game.to_owned())
                    .or_insert_with(|| SnapshotGame { modes: Vec::new() })
                    .modes
                    .push(mode.to_owned());
            }
        }
        // Forza Motorsport 5's careers (§5.3): players and profile files, no boards.
        if fi_db::dashboard::game_known_in(&mut tx, fi_domain::FM5).await? {
            let modes = snap.fm5_in(&mut tx, &MODES, &mut staging).await?;
            games.insert(fi_domain::FM5.to_owned(), SnapshotGame { modes });
        }
        tx.commit().await?;
    }
    pool.close().await;
    staging.write_json("manifest.json", &manifest(now, interval, games))?;
    let bytes = staging.bytes;
    let counts: Counts = snap.counts();
    let pruned = staging.commit(SNAPSHOTS_KEPT)?;
    if let Err(e) = pruned {
        tracing::warn!(error = %e, "dashboard snapshot swapped in; pruning old ones failed");
    }
    tracing::info!(
        boards = counts.boards,
        rows = counts.rows,
        players = counts.players,
        profiles = counts.profiles,
        clubs = counts.clubs,
        garages = counts.garages,
        creations = counts.creations,
        careers = counts.careers,
        bytes,
        took_ms = started.elapsed().as_millis() as u64,
        "dashboard snapshot written"
    );
    Ok(())
}

async fn sqlx_exec(
    conn: &mut sqlx::PgConnection,
    sql: &'static str,
) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::raw_sql(sql).execute(conn).await?;
    Ok(())
}

/// A snapshot being written: `snapshots/<name>.tmp` until [`Staging::commit`]; dropped
/// uncommitted, the folder is removed and `current` is untouched.
pub struct Staging {
    root: PathBuf,
    name: String,
    tmp: PathBuf,
    bytes: u64,
    max_bytes: u64,
    committed: bool,
}

fn mkdir_0755(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

/// fsyncs every file and folder under `dir` (folders after their contents), then `dir`.
fn sync_tree(dir: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            sync_tree(&path)?;
        } else {
            File::open(&path)?.sync_all()?;
        }
    }
    sync_dir(dir)
}

/// A relative path of generated parts only: no root, no `..`, no empty or dot component.
fn safe_relative(rel: &str) -> bool {
    !rel.is_empty()
        && Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && rel.split('/').all(|p| !p.is_empty() && !p.starts_with('.'))
}

impl Staging {
    /// Starts `root/snapshots/<name>.tmp` (a leftover of the same name is replaced) and removes
    /// leftover `.current-*.tmp` links of a crashed run.
    pub fn begin(root: &Path, name: &str, max_bytes: u64) -> std::io::Result<Staging> {
        let snapshots = root.join("snapshots");
        mkdir_0755(&snapshots)?;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let file = entry.file_name();
            let file = file.to_string_lossy();
            if file.starts_with(".current-")
                && file.ends_with(".tmp")
                && entry.file_type()?.is_symlink()
            {
                fs::remove_file(entry.path())?;
            }
        }
        let tmp = snapshots.join(format!("{name}.tmp"));
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        mkdir_0755(&tmp)?;
        Ok(Staging {
            root: root.to_owned(),
            name: name.to_owned(),
            tmp,
            bytes: 0,
            max_bytes,
            committed: false,
        })
    }

    /// Writes `bytes` at `rel` (0644; fsynced with the tree at commit), within the size guard.
    pub fn write(&mut self, rel: &str, bytes: &[u8]) -> Result<(), String> {
        if !safe_relative(rel) {
            return Err(format!("refused snapshot path {rel:?}"));
        }
        if self.bytes + bytes.len() as u64 > self.max_bytes {
            return Err(format!(
                "the snapshot passed its size guard ({} bytes); nothing was swapped in",
                self.max_bytes
            ));
        }
        let path = self.tmp.join(rel);
        let io = |e: std::io::Error| format!("{rel}: {e}");
        if let Some(parent) = path.parent() {
            mkdir_0755(parent).map_err(io)?;
        }
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&path)
            .map_err(io)?;
        f.write_all(bytes).map_err(io)?;
        f.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(io)?;
        self.bytes += bytes.len() as u64;
        Ok(())
    }

    /// Writes `value` as JSON at `rel`.
    pub fn write_json<T: serde::Serialize>(
        &mut self,
        rel: &str,
        value: &T,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let bytes = serde_json::to_vec(value)?;
        Ok(self.write(rel, &bytes)?)
    }

    /// fsyncs the tree, renames the folder into place and points `current` at it (a new symlink
    /// renamed over the old one): from there the snapshot is live. Then prunes; the returned inner
    /// result is the pruning's, which never undoes the swap.
    pub fn commit(mut self, keep: usize) -> std::io::Result<std::io::Result<()>> {
        let snapshots = self.root.join("snapshots");
        let done = snapshots.join(&self.name);
        sync_tree(&self.tmp)?;
        fs::rename(&self.tmp, &done)?;
        self.committed = true;
        sync_dir(&snapshots)?;
        let link = self.root.join(format!(".current-{}.tmp", self.name));
        if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link)?;
        }
        std::os::unix::fs::symlink(Path::new("snapshots").join(&self.name), &link)?;
        fs::rename(&link, self.root.join("current"))?;
        sync_dir(&self.root)?;
        Ok(prune(&self.root, keep))
    }
}

impl Sink for Staging {
    fn put(&mut self, rel: &str, bytes: Vec<u8>) -> Result<(), String> {
        self.write(rel, &bytes)
    }

    fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.tmp);
        }
    }
}

/// The snapshot `current` points at (its folder name), if any.
pub fn current(root: &Path) -> Option<String> {
    let target = fs::read_link(root.join("current")).ok()?;
    target.file_name()?.to_str().map(str::to_owned)
}

/// Keeps the newest `keep` snapshots (and always the current one); deletes older ones and
/// temporary folders older than an hour (a younger one may be a running job's).
pub fn prune(root: &Path, keep: usize) -> std::io::Result<()> {
    let snapshots = root.join("snapshots");
    let live = current(root);
    let now = SystemTime::now();
    let mut done = Vec::new();
    for entry in fs::read_dir(&snapshots)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if name.ends_with(".tmp") {
            let age = entry
                .metadata()?
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .unwrap_or_default();
            if age > STALE_TMP {
                fs::remove_dir_all(entry.path())?;
            }
        } else {
            done.push(name);
        }
    }
    done.sort();
    let old = done.len().saturating_sub(keep);
    for name in &done[..old] {
        if Some(name) != live.as_ref() {
            fs::remove_dir_all(snapshots.join(name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fi-snapshot-{tag}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    const T0: i64 = 1_793_491_200_000; // 2026-11-01T00:00:00Z

    /// A small snapshot generated `hours` after T0, swapped in.
    fn snapshot(root: &Path, hours: i64) -> std::io::Result<()> {
        let now = T0 + hours * 3_600_000;
        let mut s = Staging::begin(root, &folder_name(&rfc3339(now)), 1 << 20).unwrap();
        for rel in [
            "fh2/noob/boards.json",
            "fh2/noob/boards/speed-traps/1.json",
            "fh2/noob/players.json",
            "fh2/profiles/driver.json",
        ] {
            s.write(rel, b"{}").unwrap();
        }
        let mut games = BTreeMap::new();
        games.insert(
            "fh2".to_owned(),
            SnapshotGame {
                modes: vec!["noob".into()],
            },
        );
        s.write_json("manifest.json", &manifest(now, 3_600, games))
            .unwrap();
        s.commit(SNAPSHOTS_KEPT)?
    }

    #[test]
    fn folder_names_are_compact_utc() {
        assert_eq!(
            folder_name("2026-11-01T12:34:56.000000Z"),
            "20261101T123456Z"
        );
        assert_eq!(folder_name(&rfc3339(T0)), "20261101T000000Z");
    }

    #[test]
    fn the_tree_is_written_and_swapped_in() {
        let root = temp_root("tree");
        snapshot(&root, 0).unwrap();
        assert_eq!(current(&root).as_deref(), Some("20261101T000000Z"));
        let cur = root.join("current");
        for rel in [
            "manifest.json",
            "fh2/noob/boards.json",
            "fh2/noob/boards/speed-traps/1.json",
            "fh2/noob/players.json",
            "fh2/profiles/driver.json",
        ] {
            let p = cur.join(rel);
            assert!(p.is_file(), "{rel}");
            assert_eq!(mode(&p), 0o644, "{rel}");
        }
        assert_eq!(
            mode(&root.join("snapshots/20261101T000000Z/fh2/noob/boards/speed-traps")),
            0o755
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(cur.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest,
            serde_json::json!({"version": 1, "generatedAt": "2026-11-01T00:00:00.000000Z",
                "nextAt": "2026-11-01T01:00:00.000000Z", "intervalSecs": 3600,
                "games": {"fh2": {"modes": ["noob"]}}})
        );
        // The symlink is relative (the site mounts the folder elsewhere).
        assert_eq!(
            fs::read_link(&cur).unwrap(),
            Path::new("snapshots/20261101T000000Z")
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failure_leaves_current_untouched() {
        let root = temp_root("fail");
        snapshot(&root, 0).unwrap();
        // Over the size guard: refused file by file, nothing swapped, the folder removed.
        {
            let mut s = Staging::begin(&root, "20261101T010000Z", 10).unwrap();
            s.write("fh2/a.json", b"12345").unwrap();
            let err = s.write("fh2/b.json", b"123456").unwrap_err();
            assert!(err.contains("size guard"), "{err}");
            // A duplicate path is an error too, never an overwrite.
            assert!(s.write("fh2/a.json", b"1").is_err());
            assert!(s.write("../escape.json", b"{}").is_err());
            assert!(s.write("fh2/.hidden.json", b"{}").is_err());
        }
        assert!(!root.join("snapshots/20261101T010000Z.tmp").exists());
        assert!(!root.join("snapshots/20261101T010000Z").exists());
        assert!(!root.join("escape.json").exists());
        assert_eq!(current(&root).as_deref(), Some("20261101T000000Z"));
        assert!(root.join("current/manifest.json").is_file());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_last_three_are_kept_and_only_stale_temporaries_go() {
        let root = temp_root("prune");
        for hour in 0..5 {
            snapshot(&root, hour).unwrap();
        }
        // A crashed run's folder two hours old goes; a running job's fresh one stays.
        let stale = root.join("snapshots/20260101T000000Z.tmp");
        let fresh = root.join("snapshots/20261101T050000Z.tmp");
        fs::create_dir_all(&stale).unwrap();
        fs::create_dir_all(&fresh).unwrap();
        File::open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(7_200))
            .unwrap();
        prune(&root, SNAPSHOTS_KEPT).unwrap();
        let mut left: Vec<String> = fs::read_dir(root.join("snapshots"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            vec![
                "20261101T020000Z",
                "20261101T030000Z",
                "20261101T040000Z",
                "20261101T050000Z.tmp"
            ]
        );
        assert_eq!(current(&root).as_deref(), Some("20261101T040000Z"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn leftover_links_are_cleaned_and_a_prune_failure_keeps_the_swap() {
        let root = temp_root("edges");
        snapshot(&root, 0).unwrap();
        // A crashed run's temporary link is removed at the next start.
        std::os::unix::fs::symlink("snapshots/x", root.join(".current-old.tmp")).unwrap();
        snapshot(&root, 1).unwrap();
        assert!(fs::symlink_metadata(root.join(".current-old.tmp")).is_err());
        snapshot(&root, 2).unwrap();
        // An old snapshot that cannot be deleted: the next run still swaps in and succeeds.
        let locked = root.join("snapshots/20261101T000000Z/fh2");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let outcome = {
            let now = T0 + 3 * 3_600_000;
            let mut s = Staging::begin(&root, &folder_name(&rfc3339(now)), 1 << 20).unwrap();
            s.write("manifest.json", b"{}").unwrap();
            s.commit(SNAPSHOTS_KEPT)
        };
        let pruned = outcome.expect("the swap itself succeeded");
        assert_eq!(current(&root).as_deref(), Some("20261101T030000Z"));
        // Running as root removes it anyway; otherwise the prune reports its failure.
        if pruned.is_err() {
            assert!(root.join("snapshots/20261101T000000Z").exists());
        }
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    #[ignore = "needs Postgres (FI_TEST_DATABASE_URL)"]
    async fn a_synthetic_snapshot_keeps_the_games_ranks_and_leaves_hidden_profiles_out() {
        use fi_db::scores::{self, Assists, BoardFilter, BoardKey, BoardQuery, Submission};
        use fi_db::visibility::Viewer;
        let url = std::env::var("FI_TEST_DATABASE_URL").expect("FI_TEST_DATABASE_URL");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        fi_db::migrate::run(&pool).await.unwrap();
        let game = format!("s{}", &uuid::Uuid::now_v7().simple().to_string()[22..32]);
        sqlx::query("INSERT INTO game (code, title) VALUES ($1, 'Snapshot test')")
            .bind(&game)
            .execute(&pool)
            .await
            .unwrap();
        // Five Hardcore drivers on speed trap 7; the second (by speed) opts out of the website.
        let mut players = Vec::new();
        for (i, speed) in [90.0, 85.0, 80.0, 75.0, 70.0].into_iter().enumerate() {
            let (account, profile, xuid, name): (uuid::Uuid, uuid::Uuid, i64, String) =
                sqlx::query_as(
                    "WITH a AS (INSERT INTO account (origin) VALUES ('registered') RETURNING id) \
                     INSERT INTO profile (account_id, game, xuid, name, discriminator, mode, origin) \
                     SELECT a.id, $1, nextval('profile_xuid_seq'), $2, 1, 'hardcore', 'created' FROM a \
                     RETURNING account_id, id, xuid, name",
                )
                .bind(&game)
                .bind(format!("Snap{i}x{}", &game[1..]))
                .fetch_one(&pool)
                .await
                .unwrap();
            let viewer = Viewer {
                game: &game,
                mode: "hardcore",
                profile_id: profile,
                account_id: account,
                xuid,
                session_id: uuid::Uuid::nil(),
            };
            let mut c = pool.acquire().await.unwrap();
            scores::submit_in(
                &mut c,
                &viewer,
                &Submission {
                    key: BoardKey {
                        scoreboard_type: 9,
                        score_type: 5,
                        game_scoreboard_id: 7,
                        track_id: -1,
                    },
                    score: speed,
                    car_id: 247,
                    car_class_id: 3,
                    car_pi: 0.7,
                    assists: Assists::default(),
                    data: serde_json::json!({}),
                },
                uuid::Uuid::now_v7(),
            )
            .await
            .unwrap();
            players.push((account, profile, xuid, name));
        }
        // A tie on speed trap 8 between two clubs' members (SNP's player 2 first, then TWO's
        // player 4, same speed): the game ranks the earlier submission first, so only one #1.
        for i in [2usize, 4] {
            let (account, profile, xuid, _) = &players[i];
            let viewer = Viewer {
                game: &game,
                mode: "hardcore",
                profile_id: *profile,
                account_id: *account,
                xuid: *xuid,
                session_id: uuid::Uuid::nil(),
            };
            let mut c = pool.acquire().await.unwrap();
            scores::submit_in(
                &mut c,
                &viewer,
                &Submission {
                    key: BoardKey {
                        scoreboard_type: 9,
                        score_type: 5,
                        game_scoreboard_id: 8,
                        track_id: -1,
                    },
                    score: 60.0,
                    car_id: 247,
                    car_class_id: 3,
                    car_pi: 0.7,
                    assists: Assists::default(),
                    data: serde_json::json!({}),
                },
                uuid::Uuid::now_v7(),
            )
            .await
            .unwrap();
        }
        sqlx::query("UPDATE profile SET web_listed = false WHERE id = $1")
            .bind(players[1].1)
            .execute(&pool)
            .await
            .unwrap();
        // A club of three (the opted-out driver among them) and a private club.
        for (tag, privacy, members) in [
            ("SNP", 1i16, vec![0usize, 1, 2]),
            ("PVT", 4, vec![3]),
            ("TWO", 1, vec![4]),
        ] {
            let club: i32 = sqlx::query_scalar(
                "INSERT INTO game_club (game, mode, tag, name, privacy, region) \
                 VALUES ($1, 'hardcore', $2, 'Club ' || $2, $3, 3) RETURNING id",
            )
            .bind(&game)
            .bind(tag)
            .bind(privacy)
            .fetch_one(&pool)
            .await
            .unwrap();
            for (n, i) in members.into_iter().enumerate() {
                sqlx::query(
                    "INSERT INTO game_club_member (game, profile_id, club_id, role, xp) \
                     VALUES ($1, $2, $3, $4, 100)",
                )
                .bind(&game)
                .bind(players[i].1)
                .bind(club)
                .bind(if n == 0 { 1i16 } else { 3 })
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        // Shared creations: player 0 shares a livery (and keeps a private one); the opted-out
        // player 1 shares one too. Player 0 also has a garage.
        for (i, share) in [(0usize, true), (0, false), (1, true)] {
            let livery: i64 = (uuid::Uuid::now_v7().as_u128() % 1_000_000_000_000) as i64 + 1;
            sqlx::query(
                "INSERT INTO game_livery (id, game, mode, profile_id, name, car_id, number_of_layers) \
                 VALUES ($1, $2, 'hardcore', $3, 'Saved', 247, 3)",
            )
            .bind(livery)
            .bind(&game)
            .bind(players[i].1)
            .execute(&pool)
            .await
            .unwrap();
            if share {
                sqlx::query(
                    "INSERT INTO storefront_file (game, profile_id, kind, livery_id, file_type, car_id, title, \
                       description, keyword_one, keyword_two, preview) \
                     VALUES ($1, $2, 'livery', $3, 2, 247, 'Mine', '', 0, 0, '\\x89504e470d0a1a0a00'::bytea)",
                )
                .bind(&game)
                .bind(players[i].1)
                .bind(livery)
                .execute(&pool)
                .await
                .unwrap();
            }
        }
        sqlx::query(
            "INSERT INTO game_profile_table (game, profile_id, name, data) VALUES ($1, $2, 'garageCars', \
             '{\"1\": {\"carId\": 1652, \"numRaces\": 3}, \"2\": {\"carId\": 247, \"numRaces\": 7}}')",
        )
        .bind(&game)
        .bind(players[0].1)
        .execute(&pool)
        .await
        .unwrap();
        // Player 2's garage grew to 100,000 rows (a scripted client): read bounded.
        sqlx::query(
            "INSERT INTO game_profile_table (game, profile_id, name, data) \
             SELECT $1, $2, 'garageCars', jsonb_object_agg(i::text, \
               jsonb_build_object('carId', i % 3000 + 1)) \
             FROM generate_series(1, 100000) i",
        )
        .bind(&game)
        .bind(players[2].1)
        .execute(&pool)
        .await
        .unwrap();
        // §5.3: player 0 also has an FM5 career under the same name (a gamertag is global), with
        // a level and one clean best time; FH2's file of that name must survive.
        let fm5_profile: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO profile (account_id, game, xuid, name, discriminator, mode, origin) \
             VALUES ($1, 'fm5', $2, $3, 1, 'hardcore', 'created') RETURNING id",
        )
        .bind(players[0].0)
        .bind(players[0].2)
        .bind(&players[0].3)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO profile_progress (profile_id, level, xp) VALUES ($1, 3, 1200)")
            .bind(fm5_profile)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO fm5_score (profile_id, score_type, scoreboard_type, game_scoreboard_id, \
               track_id, car_id, car_class_id, car_drive_type_id, score, is_clean, score_id, data) \
             VALUES ($1, 0, 2, -1, 31, 500, 4, 1, 42.5, true, $2, '{}')",
        )
        .bind(fm5_profile)
        .bind(uuid::Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
        let fm5_modes;
        let refs = RefKey::new("snapshot-test-key").unwrap();
        let root = temp_root("pg");
        let mut staging = Staging::begin(&root, "20261101T000000Z", SNAPSHOT_MAX_BYTES).unwrap();
        let mut snap = Snapshotter::new(&refs, SNAPSHOT_MAX_BYTES);
        let month = fi_surfaces::dashboard::boards::current_month();
        {
            let mut tx = pool.begin().await.unwrap();
            sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await
                .unwrap();
            for mode in MODES {
                snap.mode_in(&mut tx, &game, mode, month, &mut staging)
                    .await
                    .unwrap();
            }
            fm5_modes = snap.fm5_in(&mut tx, &MODES, &mut staging).await.unwrap();
            tx.commit().await.unwrap();
        }
        staging
            .write_json("manifest.json", &manifest(T0, 3_600, BTreeMap::new()))
            .unwrap();
        let bytes = staging.bytes;
        staging.commit(SNAPSHOTS_KEPT).unwrap().unwrap();
        let counts = snap.counts();
        // The game's own ranks (GetRows), for everyone visible.
        let mut c = pool.acquire().await.unwrap();
        let (account, profile, xuid, _) = &players[0];
        let page = scores::board_in(
            &mut c,
            &Viewer {
                game: &game,
                mode: "hardcore",
                profile_id: *profile,
                account_id: *account,
                xuid: *xuid,
                session_id: uuid::Uuid::nil(),
            },
            &BoardQuery {
                scoreboard_type: 9,
                score_type: 5,
                filter: BoardFilter::Event(7),
                circle: None,
                start: 1,
                max: 500,
                near_me: false,
            },
        )
        .await
        .unwrap();
        let expected: Vec<(u64, String)> = page
            .rows
            .iter()
            .filter(|r| r.player.name.as_deref() != Some(players[1].3.as_str()))
            .map(|r| (r.position as u64, r.player.name.clone().unwrap()))
            .collect();
        let cur = root.join("current").join(&game);
        let read = |rel: &str| -> serde_json::Value {
            serde_json::from_slice(&fs::read(cur.join(rel)).unwrap()).unwrap()
        };
        let board = read("hardcore/boards/speed-traps/7.json");
        let got: Vec<(u64, String)> = board["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["rank"].as_u64().unwrap(),
                    r["player"]["name"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(got, expected);
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            vec![1, 3, 4, 5]
        );
        assert_eq!(board["board"]["entries"], 4);
        // Refs: on every row and on the top, equal to the key's HMAC of the profile id.
        let ref_of = |i: usize| refs.of(players[i].1);
        assert_eq!(board["board"]["top"]["ref"], ref_of(0).as_str());
        assert_eq!(board["rows"][0]["player"]["ref"], ref_of(0).as_str());
        let index = read("hardcore/boards.json");
        let traps = &index["families"][0]["boards"][6];
        assert_eq!(traps["board"], "7");
        assert_eq!(
            traps["refs"],
            serde_json::json!([ref_of(0), ref_of(2), ref_of(3), ref_of(4)])
        );
        // Every catalog board has a file, empty ones included.
        assert!(cur.join("hardcore/boards/speed-traps/1.json").is_file());
        assert!(cur.join("noob/boards/rivals/560.json").is_file());
        // Players and profiles: the listed ones only, with refs and their game ranks.
        let list = read("hardcore/players.json");
        let rows = list["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r["name"] != players[1].3.as_str()));
        assert!(
            rows.iter()
                .all(|r| r["ref"].as_str().is_some_and(|s| s.len() == 22))
        );
        assert!(
            !cur.join(format!("profiles/{}.json", players[1].3.to_lowercase()))
                .exists()
        );
        let third = read(&format!("profiles/{}.json", players[2].3.to_lowercase()));
        assert_eq!(third["records"], 1, "the tie on trap 8 is its own");
        assert_eq!(third["bests"][0]["rank"], 3);
        assert_eq!(third["card"]["ref"], ref_of(2).as_str());
        let first = read(&format!("profiles/{}.json", players[0].3.to_lowercase()));
        assert_eq!(first["records"], 1);
        // No identifier beyond the name and the ref anywhere.
        for rel in [
            "hardcore/players.json",
            "hardcore/boards/speed-traps/7.json",
        ] {
            let text = fs::read_to_string(cur.join(rel)).unwrap();
            assert!(!text.contains(&players[0].2.to_string()), "xuid in {rel}");
            assert!(
                !text.contains(&players[0].1.to_string()),
                "profile id in {rel}"
            );
        }
        // Clubs: the private one is never written; the open one counts all its members but lists
        // only the website-listed ones; its records are the boards a member holds.
        let clubs = read("hardcore/clubs.json");
        let tags: Vec<&str> = clubs["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["tag"].as_str().unwrap())
            .collect();
        assert_eq!(tags, vec!["SNP", "TWO"]);
        assert!(!cur.join("hardcore/clubs/PVT.json").exists());
        let club = read("hardcore/clubs/SNP.json");
        assert_eq!(club["card"]["members"], 3);
        assert_eq!(club["card"]["xp"], 300.0);
        assert_eq!(club["card"]["region"], "europe");
        assert_eq!(club["card"]["leader"]["ref"], ref_of(0).as_str());
        assert_eq!(club["privacy"], "open");
        let roster: Vec<&str> = club["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(roster, vec![players[0].3.as_str(), players[2].3.as_str()]);
        let record = &club["records"][0];
        assert_eq!(
            (record["family"].as_str(), record["board"].as_str()),
            (Some("speed-traps"), Some("7"))
        );
        assert_eq!(record["holder"], players[0].3.as_str());
        assert_eq!(record["ref"], ref_of(0).as_str());
        // SNP holds speed trap 7 and, on the tie, speed trap 8; TWO holds nothing.
        let held: Vec<(String, String)> = club["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["board"].as_str().unwrap().to_owned(),
                    r["holder"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(
            held,
            vec![
                ("7".to_owned(), players[0].3.clone()),
                ("8".to_owned(), players[2].3.clone())
            ]
        );
        let two = read("hardcore/clubs/TWO.json");
        assert_eq!(two["records"], serde_json::json!([]));
        // The live route credits the same single #1 (position 1 is unique: the game's order
        // breaks ties by the earlier submission).
        let live = fi_surfaces::dashboard::clubs::board_tops_in(&mut c, &game, "hardcore", month)
            .await
            .unwrap();
        let trap8: Vec<&str> = live
            .iter()
            .filter(|t| t.fam.slug() == "speed-traps" && t.board.summary.board == "8")
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(trap8, vec![players[2].3.as_str()]);
        // Creations and garages (§7): the listed author's shared livery only, with an opaque id.
        let creations = read("hardcore/creations.json");
        let rows = creations["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{creations}");
        assert_eq!(rows[0]["author"]["name"], players[0].3.as_str());
        assert_eq!(rows[0]["author"]["ref"], ref_of(0).as_str());
        assert_eq!(rows[0]["preview"], true);
        // The server-only storefront id the site sends back for the preview (stripped like refs).
        assert!(rows[0]["file"].as_i64().is_some_and(|f| f > 0));
        assert_eq!(rows[0]["car"]["name"], "Toyota 2000 GT 1969");
        let id = rows[0]["id"].as_str().unwrap();
        assert_eq!(id.len(), 22);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(read("noob/creations.json")["rows"], serde_json::json!([]));
        let garage = read(&format!(
            "profiles/{}.garage.json",
            players[0].3.to_lowercase()
        ));
        let cars: Vec<i64> = garage["cars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["car"]["id"].as_i64().unwrap())
            .collect();
        assert_eq!(cars, vec![247, 1652], "class D before the Caterham");
        assert_eq!(garage["cars"][0]["races"], 7);
        assert_eq!(garage["creations"][0]["id"], id);
        assert!(
            !cur.join(format!(
                "profiles/{}.garage.json",
                players[4].3.to_lowercase()
            ))
            .exists(),
            "no garage, no creations: no file"
        );
        // The 100,000-row garage: catalog cars only, one entry each, a bounded file.
        let huge = cur.join(format!(
            "profiles/{}.garage.json",
            players[2].3.to_lowercase()
        ));
        let size = fs::metadata(&huge).unwrap().len();
        let garage: serde_json::Value = serde_json::from_slice(&fs::read(&huge).unwrap()).unwrap();
        let cars = garage["cars"].as_array().unwrap();
        assert!(
            !cars.is_empty() && cars.len() <= fi_surfaces::dashboard::garages::catalog_cars().len(),
            "{}",
            cars.len()
        );
        let mut seen = std::collections::HashSet::new();
        assert!(
            cars.iter()
                .all(|c| seen.insert(c["car"]["id"].as_i64().unwrap()))
        );
        assert!(size < 64 * 1024, "{size} bytes");
        eprintln!("SNAPSHOT huge garage: {} cars, {size} bytes", cars.len());
        assert!(
            !cur.join(format!(
                "profiles/{}.garage.json",
                players[1].3.to_lowercase()
            ))
            .exists()
        );
        assert_eq!((counts.creations, counts.garages), (1, 2));
        assert_eq!(counts.clubs, 2);
        // No player names a private club.
        let profile = read(&format!("profiles/{}.json", players[3].3.to_lowercase()));
        assert!(profile["card"].get("club").is_none());
        assert_eq!(counts.profiles, 4);
        // §5.3 FM5: the career file, its card in fm5/players.json, the mode in the manifest.
        let fm5_name = players[0].3.to_lowercase();
        let career: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join(format!("current/fm5/profiles/{fm5_name}.json"))).unwrap(),
        )
        .unwrap();
        // The mode as stored (the database keeps one mode per XUID across games).
        assert!(
            fm5_modes
                .iter()
                .any(|m| career["card"]["mode"] == m.as_str())
        );
        assert_eq!(
            career["card"]["level"], 3,
            "FM5's level as stored, not FH2's curve"
        );
        assert_eq!(
            career["progress"],
            serde_json::json!({"level": 3, "xp": 1200})
        );
        assert_eq!(career["card"]["ref"], refs.of(fm5_profile).as_str());
        let best = &career["bests"][0];
        assert_eq!(
            (
                best["kind"].as_str(),
                best["trackId"].as_i64(),
                best["score"].as_f64()
            ),
            (Some("time"), Some(31), Some(42.5))
        );
        assert_eq!(best["clean"], true);
        let fm5_players: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("current/fm5/players.json")).unwrap())
                .unwrap();
        assert!(
            fm5_players["rows"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["name"] == players[0].3.as_str())
        );
        assert!(counts.careers >= 1);
        eprintln!("SNAPSHOT bytes={bytes} counts={counts:?}");
        fs::remove_dir_all(&root).unwrap();
    }
}
