//! The gate's job (ADR 0015 §4, GATE_API.md §5):
//!
//! - `gate-sweep [--every <secs>]`: ends every stale play session of every game and pool
//!   (`max_age`, no signal for 60 minutes whatever the lease, lapsed on both the heartbeat and the
//!   game-request signal, queued and unpolled for 90 s), each with its `session.ended`, in
//!   transactions of at most 500 rows (`fi_db::gate::sweep`). Cloud Scheduler runs it hourly in
//!   production (the owner's backstop); locally `--every 5` loops until SIGTERM, logging and
//!   continuing on database errors. It replaces `sessions-reap`.
//! - `freeroam-purge`: deletes the in-game free-roam sessions and tickets that expired over an
//!   hour ago (`fi_db::freeroam::purge`, MATCHMAKING_API.md §4a). Needs a `fi_maintenance` login
//!   (`fi_jobs`): the runtime login has no DELETE. Expiry itself is a time filter, so the purge
//!   only reclaims space; each profile keeps at most 16 live of each.
//! - `online-capacity --game <game> [--set <n> | --unlimited]` (OD-71, GATE_API.md §2 "Online
//!   capacity"): shows or sets the game's online capacity, the most active sessions across its
//!   pools (1 to 100,000; `--unlimited` clears it). The gate reads it on its next decision;
//!   lowering it ends no session. As `DATABASE_URL` (the jobs login, which holds the column's
//!   UPDATE grant); the admin route `PUT /core/v1/admin/capacity` is the audited way. Its status
//!   line also shows the global capacity.
//! - `online-capacity --global [--set <n> | --unlimited]` (OD-228, GATE_API.md §2 "Global online
//!   capacity"): shows or sets the global online capacity, the most active sessions across every
//!   game (1 to 100,000; `--unlimited` clears it), on top of each game's and each pool's. Read on
//!   the next admission decision of any game; lowering it ends no session.
//! - `cheat-report [--days N]` (OD-147, MATCHMAKING_API.md §3.9): the peer observations of the last
//!   days per kind (counts, value percentiles), speed per observed car against its own limit (NR-5),
//!   the quorums of the last 24 hours and the live cheat flags, the
//!   calibration data for `WatchRules` before the cheater pool ships, and the relay detector's reports
//!   per relay, type, kind and outcome with each relay start's counters (§5d). Ids and numbers only, never a
//!   name, a gamertag or an XUID. Flags lifted within those days are listed too, marked.
//! - `cheat-unflag --profile <uuid> --reason <text> [--actor <name>]` (MATCHMAKING_API.md §3.9
//!   "Lifting a flag"): the operator's lift, the same database function as the moderators'
//!   `DELETE /core/v1/admin/cheat-flags/{profileId}`: a live flag ends now and is marked lifted (the
//!   next flag does not double on it), with an operator `admin_action` row `profile.cheat_unflag`
//!   (actor `admin:<name>`, default `admin:fi-cli`). Idempotent: no live flag changes nothing but
//!   the audit row. As `DATABASE_URL`, a fi_maintenance login such as fi_jobs.

use super::Outcome;
use fi_db::{DbConfig, Workload, admission::Reaped, freeroam, gate};
use std::time::Duration;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  gate-sweep       end stale play sessions of every game (DATABASE_URL); --every <secs> loops
  freeroam-purge   delete free-roam sessions and tickets expired over an hour ago (DATABASE_URL,
                   a fi_maintenance login such as fi_jobs)
  online-capacity  --game fh2 [--set <n> | --unlimited]: show or set the game's online capacity,
                   the most players online at once across its pools (DATABASE_URL; OD-71);
                   --global [--set <n> | --unlimited]: the same across every game (OD-228)
  idle-report      --game fh2 [--days N]: idle-in-game episodes per tier (OD-121, observation)
  cheat-report     [--days N]: peer observations per kind, quorums and live cheat flags (OD-147;
                   DATABASE_URL, a fi_maintenance login such as fi_jobs)
  cheat-unflag     --profile <uuid> --reason <text> [--actor <name>]: lift a profile's cheat flag
                   now, audited (MATCHMAKING_API.md §3.9; DATABASE_URL, a fi_maintenance login)";

/// Runs `command` with `args`; `None` when it is not a gate command.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    match command {
        "gate-sweep" => Some(gate_sweep(args).await),
        "freeroam-purge" => Some(freeroam_purge(args).await),
        "online-capacity" => Some(online_capacity(args).await),
        "idle-report" => Some(idle_report(args).await),
        "cheat-report" => Some(cheat_report(args).await),
        "cheat-unflag" => Some(cheat_unflag(args).await),
        _ => None,
    }
}

/// `freeroam-purge`.
async fn freeroam_purge(args: &[String]) -> Outcome {
    if !args.is_empty() {
        return Err("usage: freeroam-purge".into());
    }
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-freeroam-purge", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let purged = freeroam::purge(&pool, 3_600).await;
    pool.close().await;
    let purged = purged?;
    tracing::info!(
        sessions = purged.sessions,
        tickets = purged.tickets,
        "free-roam purge"
    );
    Ok(())
}

/// What `online-capacity` does.
#[derive(Debug, PartialEq, Eq)]
enum CapacityChange {
    Show,
    Set(Option<u32>),
}

/// Whose capacity `online-capacity` shows or sets.
#[derive(Debug, PartialEq, Eq)]
enum CapacityTarget {
    /// One game's (OD-71).
    Game(String),
    /// The global one across every game (OD-228).
    Global,
}

fn capacity_args(args: &[String]) -> Result<(CapacityTarget, CapacityChange), String> {
    let usage =
        "usage: online-capacity (--game <game> | --global) [--set <1..100000> | --unlimited]";
    let (mut target, mut change) = (None, CapacityChange::Show);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--game" if target.is_none() => {
                target = Some(CapacityTarget::Game(it.next().ok_or(usage)?.clone()));
            }
            "--global" if target.is_none() => target = Some(CapacityTarget::Global),
            "--set" if change == CapacityChange::Show => {
                let n = it
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|n| (1..=100_000).contains(n))
                    .ok_or("--set takes 1 to 100000")?;
                change = CapacityChange::Set(Some(n));
            }
            "--unlimited" if change == CapacityChange::Show => change = CapacityChange::Set(None),
            _ => return Err(usage.into()),
        }
    }
    Ok((target.ok_or(usage)?, change))
}

/// "unlimited" or the number.
fn capacity_text(capacity: Option<u32>) -> String {
    capacity.map_or_else(|| "unlimited".to_owned(), |c| c.to_string())
}

/// `online-capacity (--game <game> | --global) [--set <n> | --unlimited]`.
async fn online_capacity(args: &[String]) -> Outcome {
    let (target, change) = capacity_args(args)?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-online-capacity", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let mut conn = pool.acquire().await?;
    let game = match target {
        CapacityTarget::Game(game) => game,
        CapacityTarget::Global => {
            if let CapacityChange::Set(capacity) = change {
                let previous = gate::set_global_online_capacity_in(&mut conn, capacity).await?;
                tracing::info!(from = ?previous, to = ?capacity, "global online capacity set");
            }
            let load = gate::global_online_load_in(&mut conn).await?;
            println!(
                "online-capacity: global capacity {}, {} active, {} queued (every game)",
                capacity_text(load.capacity),
                load.active,
                load.queued
            );
            return Ok(());
        }
    };
    if let CapacityChange::Set(capacity) = change {
        let previous = gate::set_online_capacity_in(&mut conn, &game, capacity)
            .await?
            .ok_or_else(|| format!("unknown game {game:?}"))?;
        tracing::info!(game = %game, from = ?previous, to = ?capacity, "online capacity set");
    }
    let load = gate::online_load_in(&mut conn, &game)
        .await?
        .ok_or_else(|| format!("unknown game {game:?}"))?;
    let global = gate::global_online_capacity_in(&mut conn).await?;
    println!(
        "online-capacity: {game} capacity {}, {} active, {} queued; global capacity {}",
        capacity_text(load.capacity),
        load.active,
        load.queued,
        capacity_text(global)
    );
    Ok(())
}

/// `idle-report --game <game> [--days <1..90>]` (OD-121, GATE_API.md §4a): the idle episodes the
/// observation recorded, per tier.
async fn idle_report(args: &[String]) -> Outcome {
    let usage = "usage: idle-report --game <game> [--days <1..90>]";
    let (mut game, mut days) = (None, 7);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--game" => game = Some(it.next().ok_or(usage)?.clone()),
            "--days" => {
                days = it
                    .next()
                    .and_then(|v| v.parse::<i32>().ok())
                    .filter(|d| (1..=90).contains(d))
                    .ok_or("--days takes 1 to 90")?;
            }
            _ => return Err(usage.into()),
        }
    }
    let game = game.ok_or(usage)?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-idle-report", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let rows = fi_db::activity::observations(&pool, &game, days).await?;
    println!("idle-report: {game}, last {days} days (observation; nobody was evicted)");
    if rows.is_empty() {
        println!("  no idle episodes");
    }
    for r in rows {
        println!(
            "  {:<4} {} episodes, {} sessions; idle p50 {}s p90 {}s; menu p50 {}s; waiting p50 {}",
            r.tier, r.episodes, r.sessions, r.idle_p50, r.idle_p90, r.menu_p50, r.waiting_p50
        );
    }
    Ok(())
}

/// `cheat-report [--days <1..30>]` (OD-147, MATCHMAKING_API.md §3.9): what the peer observations
/// saw, per kind, and the live flags (ids only).
async fn cheat_report(args: &[String]) -> Outcome {
    let days = match args {
        [] => 7,
        [flag, value] if flag == "--days" => value
            .parse::<i32>()
            .ok()
            .filter(|d| (1..=30).contains(d))
            .ok_or("--days takes 1 to 30")?,
        _ => return Err("usage: cheat-report [--days <1..30>]".into()),
    };
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-cheat-report", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let report = fi_db::observations::report(&pool, days).await;
    // MATCHMAKING_API.md §5d (CR-S80): the relay detector's reports and its starts' counters.
    let relay = fi_db::relay_reports::report(&pool, days).await;
    pool.close().await;
    print!("{}", cheat_lines(&report?, days));
    let (counts, starts) = relay?;
    print!("{}", relay_lines(&counts, &starts));
    Ok(())
}

/// The arguments of `cheat-unflag`.
#[derive(Debug, PartialEq, Eq)]
struct UnflagArgs {
    profile: uuid::Uuid,
    reason: String,
    actor: String,
}

fn unflag_args(args: &[String]) -> Result<UnflagArgs, String> {
    let usage = "usage: cheat-unflag --profile <uuid> --reason <text> [--actor <name>]";
    let (mut profile, mut reason, mut actor) = (None, None, None);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next().ok_or(usage)?;
        match flag.as_str() {
            "--profile" if profile.is_none() => {
                profile = Some(
                    uuid::Uuid::parse_str(value).map_err(|_| "--profile takes a profile UUID")?,
                );
            }
            "--reason" if reason.is_none() => reason = Some(value.clone()),
            "--actor" if actor.is_none() => actor = Some(value.clone()),
            _ => return Err(usage.into()),
        }
    }
    let reason = reason.ok_or(usage)?;
    // The moderators' reason rule (CORE_API.md §5): 1 to 500 characters, no control characters.
    if reason.trim().is_empty()
        || reason.chars().count() > 500
        || reason.chars().any(char::is_control)
    {
        return Err("--reason takes 1 to 500 characters, no control characters".into());
    }
    let actor = actor.unwrap_or_else(|| "fi-cli".into());
    if !(1..=128).contains(&actor.len())
        || !actor
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@-".contains(&b))
    {
        return Err("--actor takes 1 to 128 of A-Z a-z 0-9 . _ @ -".into());
    }
    Ok(UnflagArgs {
        profile: profile.ok_or(usage)?,
        reason,
        actor,
    })
}

/// `cheat-unflag --profile <uuid> --reason <text> [--actor <name>]`.
async fn cheat_unflag(args: &[String]) -> Outcome {
    let a = unflag_args(args)?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-cheat-unflag", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let lifted =
        fi_db::observations::unflag_as_operator(&pool, a.profile, &a.actor, &a.reason).await;
    pool.close().await;
    let lifted = match lifted {
        Err(fi_db::identity::IdentityError::NotFound) => {
            return Err(format!("no live profile {}", a.profile).into());
        }
        other => other?,
    };
    tracing::info!(profile_id = %a.profile, was_flagged = lifted.was_flagged, "cheat flag lift");
    println!("{}", unflag_line(a.profile, &lifted));
    Ok(())
}

fn unflag_line(profile: uuid::Uuid, l: &fi_db::observations::Unflagged) -> String {
    match (l.was_flagged, l.episode) {
        (true, Some(episode)) => format!(
            "cheat-unflag: profile {profile} lifted (episode {episode}, was until {}); audited",
            l.until.map_or(0, |u| u.millis())
        ),
        _ => format!("cheat-unflag: profile {profile} had no flag in force; audited"),
    }
}

/// The relay section (§5d): reports per relay, type, kind and outcome; each recent start's
/// counters. Relay node keys are public keys (shortened); ids and numbers only.
fn relay_lines(
    counts: &[fi_db::relay_reports::ReportCount],
    starts: &[fi_db::relay_reports::StartCounters],
) -> String {
    let short = |k: &str| k.chars().take(8).collect::<String>();
    let mut out = format!(
        "  relay detector ({}): {} report lines, {} relay starts\n",
        if fi_db::relay_reports::RELAY_REPORTS_RECORD_ONLY {
            "relay-reports-record-only: nothing flagged"
        } else {
            "flagging"
        },
        counts.len(),
        starts.len()
    );
    for c in counts {
        out.push_str(&format!(
            "  relay {} {} {} {}{}: {} reports about {} profiles\n",
            short(&c.relay_key),
            c.kind_type,
            c.kind,
            c.outcome,
            c.reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default(),
            c.reports,
            c.profiles
        ));
    }
    for s in starts {
        let c = &s.counters;
        out.push_str(&format!(
            "  relay {} start: {} batches, {} cars decoded, {} undecodable, {} foreign owners, {} XUID mismatches, {} without a ring, {} without rules, {} unevaluable, {} dropped, {} rings in use, last {}\n",
            short(&s.relay_key),
            s.batches,
            c.decoded,
            c.undecodable,
            c.foreign_owner,
            c.xuid_mismatch,
            c.witness_full,
            c.rules_missing,
            c.rule_unevaluable,
            c.report_dropped,
            c.rings_in_use,
            s.updated_at.millis()
        ));
    }
    out
}

fn cheat_lines(r: &fi_db::observations::Report, days: i32) -> String {
    let num = |v: Option<f64>| v.map_or("-".to_owned(), |v| format!("{v:.1}"));
    let mut out = format!(
        "cheat-report: last {days} days (rules v{}; the pool filter is not live)\n",
        fi_db::observations::current().version
    );
    for k in &r.kinds {
        out.push_str(&format!(
            "  {:<6} {} observations ({} counting) by {} reporters about {} players; value p50 {} p95 {} p99 {} max {}\n",
            k.kind,
            k.observations,
            k.counting,
            k.reporters,
            k.observed,
            num(k.p50),
            num(k.p95),
            num(k.p99),
            num(k.max)
        ));
    }
    // NR-5: speed per observed car, against that car's own limit (no car: the flat one).
    for c in &r.speed_cars {
        out.push_str(&format!(
            "  speed car {} limit {:.1}: {} observations ({} counting), max {}\n",
            c.car_id.map_or("-".to_owned(), |c| c.to_string()),
            c.limit_kmh,
            c.observations,
            c.counting,
            num(c.max_kmh)
        ));
    }
    let lifted = r.flags.iter().filter(|f| f.lifted).count();
    out.push_str(&format!(
        "  {} world and kind quorums in the last 24 hours; {} live flags, {} lifted\n",
        r.quorums,
        r.flags.len() - lifted,
        lifted
    ));
    for f in &r.flags {
        out.push_str(&format!(
            "  flag profile {} until {} episode {} since {} ({}){}\n",
            f.profile_id,
            f.until.millis(),
            f.episode,
            f.first_at.millis(),
            f.kinds.join(", "),
            if f.lifted { " lifted" } else { "" }
        ));
    }
    out
}

/// `--every <secs>` (`5` or `5s`, 1 to 3600), when given.
fn every(args: &[String]) -> Result<Option<Duration>, Box<dyn std::error::Error>> {
    match args {
        [] => Ok(None),
        [flag, value] if flag == "--every" => {
            let secs: u64 = value
                .trim()
                .trim_end_matches('s')
                .parse()
                .ok()
                .filter(|s| (1..=3_600).contains(s))
                .ok_or("--every takes 1 to 3600 seconds")?;
            Ok(Some(Duration::from_secs(secs)))
        }
        _ => Err("usage: gate-sweep [--every <secs>]".into()),
    }
}

fn log(r: &Reaped) {
    tracing::info!(
        expired = r.expired,
        silent = r.silent,
        queue_left = r.queue_left,
        max_age = r.max_age,
        "gate sweep"
    );
}

/// `gate-sweep [--every <secs>]`.
async fn gate_sweep(args: &[String]) -> Outcome {
    let every = every(args)?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-gate-sweep", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let Some(period) = every else {
        let swept = gate::sweep(&pool).await;
        pool.close().await;
        log(&swept?);
        return Ok(());
    };
    tracing::info!(every_secs = period.as_secs(), "gate sweeper started");
    let mut shutdown = std::pin::pin!(fi_runtime::http::shutdown_signal());
    loop {
        match gate::sweep(&pool).await {
            Ok(r) if r.total() > 0 => log(&r),
            Ok(_) => tracing::debug!("nothing to sweep"),
            // A restarting database, a lock timeout: the next pass retries.
            Err(e) => tracing::warn!(error = %e, "gate sweep failed; retrying next pass"),
        }
        tokio::select! {
            () = tokio::time::sleep(period) => {}
            () = &mut shutdown => break,
        }
    }
    pool.close().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_capacity_arguments() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let fh2 = || CapacityTarget::Game("fh2".into());
        assert_eq!(
            capacity_args(&args(&["--game", "fh2"])).unwrap(),
            (fh2(), CapacityChange::Show)
        );
        assert_eq!(
            capacity_args(&args(&["--game", "fh2", "--set", "60"])).unwrap(),
            (fh2(), CapacityChange::Set(Some(60)))
        );
        assert_eq!(
            capacity_args(&args(&["--unlimited", "--game", "fh2"])).unwrap(),
            (fh2(), CapacityChange::Set(None))
        );
        // OD-228: the global cap.
        assert_eq!(
            capacity_args(&args(&["--global"])).unwrap(),
            (CapacityTarget::Global, CapacityChange::Show)
        );
        assert_eq!(
            capacity_args(&args(&["--global", "--set", "100"])).unwrap(),
            (CapacityTarget::Global, CapacityChange::Set(Some(100)))
        );
        assert_eq!(
            capacity_args(&args(&["--unlimited", "--global"])).unwrap(),
            (CapacityTarget::Global, CapacityChange::Set(None))
        );
        assert!(capacity_args(&args(&["--global", "--game", "fh2"])).is_err());
        assert!(capacity_args(&args(&["--game", "fh2", "--global"])).is_err());
        assert!(capacity_args(&args(&["--global", "--set", "0"])).is_err());
        assert!(capacity_args(&args(&["--game", "fh2", "--set", "0"])).is_err());
        assert!(capacity_args(&args(&["--game", "fh2", "--set", "100001"])).is_err());
        assert!(capacity_args(&args(&["--game", "fh2", "--set", "5", "--unlimited"])).is_err());
        assert!(capacity_args(&args(&["--set", "5"])).is_err());
    }

    #[test]
    fn the_cheat_report_prints_numbers_and_ids_only() {
        use fi_db::observations::{CarSpeed, FlagRow, KindStats, Report};
        let r = Report {
            kinds: vec![KindStats {
                kind: "speed".into(),
                observations: 3,
                counting: 2,
                reporters: 2,
                observed: 1,
                p50: Some(452.0),
                p95: Some(480.25),
                p99: Some(480.25),
                max: Some(481.0),
            }],
            speed_cars: vec![
                CarSpeed {
                    car_id: Some(2176),
                    limit_kmh: 311.3,
                    observations: 2,
                    counting: 1,
                    max_kmh: Some(400.0),
                },
                CarSpeed {
                    car_id: None,
                    limit_kmh: 451.0,
                    observations: 1,
                    counting: 0,
                    max_kmh: None,
                },
            ],
            quorums: 1,
            flags: vec![
                FlagRow {
                    profile_id: "00000000-0000-0000-0000-000000000000".parse().unwrap(),
                    until: fi_domain::UnixMs::from_millis(5),
                    kinds: vec!["speed".into()],
                    episode: 2,
                    first_at: fi_domain::UnixMs::from_millis(3),
                    lifted: false,
                },
                FlagRow {
                    profile_id: "00000000-0000-0000-0000-000000000001".parse().unwrap(),
                    until: fi_domain::UnixMs::from_millis(4),
                    kinds: vec!["climb".into(), "speed".into()],
                    episode: 1,
                    first_at: fi_domain::UnixMs::from_millis(2),
                    lifted: true,
                },
            ],
        };
        let text = cheat_lines(&r, 7);
        assert!(text.contains("speed  3 observations (2 counting) by 2 reporters about 1 players"));
        assert!(text.contains("p95 480.2") || text.contains("p95 480.3"));
        assert!(
            text.contains("speed car 2176 limit 311.3: 2 observations (1 counting), max 400.0")
        );
        assert!(text.contains("speed car - limit 451.0: 1 observations (0 counting), max -"));
        assert!(
            text.contains("1 world and kind quorums in the last 24 hours; 1 live flags, 1 lifted")
        );
        assert!(text.contains(
            "flag profile 00000000-0000-0000-0000-000000000000 until 5 episode 2 since 3 (speed)\n"
        ));
        assert!(text.contains(
            "flag profile 00000000-0000-0000-0000-000000000001 until 4 episode 1 since 2 (climb, speed) lifted\n"
        ));
    }

    #[test]
    fn cheat_unflag_arguments() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let id = "0190f3a2-0000-7000-8000-000000000001";
        assert_eq!(
            unflag_args(&args(&["--profile", id, "--reason", "appeal upheld"])).unwrap(),
            UnflagArgs {
                profile: id.parse().unwrap(),
                reason: "appeal upheld".into(),
                actor: "fi-cli".into(),
            }
        );
        assert_eq!(
            unflag_args(&args(&[
                "--reason",
                "r",
                "--actor",
                "alex",
                "--profile",
                id
            ]))
            .unwrap()
            .actor,
            "alex"
        );
        for bad in [
            &["--profile", id][..],
            &["--reason", "r"],
            &["--profile", "nope", "--reason", "r"],
            &["--profile", id, "--reason", " "],
            &["--profile", id, "--reason", "a\nb"],
            &["--profile", id, "--reason", "r", "--actor", "bad actor"],
            &["--profile", id, "--reason", "r", "--profile", id],
            &["--profile", id, "--reason"],
            &["--profile", id, "--reason", "r", "--extra", "x"],
        ] {
            assert!(unflag_args(&args(bad)).is_err(), "{bad:?}");
        }
        assert!(unflag_args(&args(&["--profile", id, "--reason", &"x".repeat(501)])).is_err());
        let lifted = fi_db::observations::Unflagged {
            was_flagged: true,
            episode: Some(3),
            until: Some(fi_domain::UnixMs::from_millis(9)),
        };
        assert_eq!(
            unflag_line(id.parse().unwrap(), &lifted),
            format!("cheat-unflag: profile {id} lifted (episode 3, was until 9); audited")
        );
    }

    #[test]
    fn the_relay_section_prints_counts_per_outcome_and_starts() {
        use fi_db::relay_reports::{Counters, ReportCount, StartCounters};
        let key = "1EEeJWDweWn7PHOmTQ+j1ikK1IYLmftD8Vu9A5/AjJA=".to_owned();
        let counts = vec![ReportCount {
            relay_key: key.clone(),
            kind_type: "forgery".into(),
            kind: "speed".into(),
            outcome: "would_flag_host".into(),
            reason: None,
            reports: 2,
            profiles: 1,
        }];
        let starts = vec![StartCounters {
            relay_key: key,
            batches: 4,
            counters: Counters {
                decoded: 900,
                rings_in_use: 3,
                ..Counters::default()
            },
            updated_at: fi_domain::UnixMs::from_millis(9),
        }];
        let text = relay_lines(&counts, &starts);
        assert!(text.contains("relay-reports-record-only: nothing flagged"));
        assert!(
            text.contains(
                "relay 1EEeJWDw forgery speed would_flag_host: 2 reports about 1 profiles"
            )
        );
        assert!(text.contains("4 batches, 900 cars decoded"));
        assert!(!text.contains("AjJA="), "keys shortened");
    }

    #[test]
    fn every_parses_the_local_loop() {
        assert_eq!(every(&[]).unwrap(), None);
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            every(&args(&["--every", "5s"])).unwrap(),
            Some(Duration::from_secs(5))
        );
        assert!(every(&args(&["--every", "0"])).is_err());
        assert!(every(&args(&["--every", "3601"])).is_err());
        assert!(every(&args(&["--once"])).is_err());
    }
}
