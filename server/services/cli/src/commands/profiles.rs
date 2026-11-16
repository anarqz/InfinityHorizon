//! `profiles duplicates` (CR-S98, OD-207; docs/contracts/CAREERS.md §2): the dry-run report of the
//! one-profile-per-mode cleanup. Every account that holds more than one live profile of a mode,
//! each profile with its rank (1: the one kept, MULTI_GAME_PROFILE_STUDY.md §8.1: highest XP, then
//! play time, then the newest save, then the oldest) and the values it was ranked on. Reads only
//! (`fi_db::accounts::duplicate_profiles`, one read-only transaction); prints ids and numbers,
//! never names. The cleanup itself is a separate, owner-authorized step.

use super::Outcome;
use fi_db::{DbConfig, Workload, accounts};

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  profiles duplicates  report the accounts holding more than one live profile of a mode, the
                   profile kept first (study 8.1 ranking); reads only (DATABASE_URL)";

/// Runs `command` with `args`; `None` when it is not this module's command.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    if command != "profiles" {
        return None;
    }
    Some(match args {
        [sub] if sub == "duplicates" => duplicates().await,
        _ => Err("profiles takes one subcommand: duplicates".into()),
    })
}

/// One report line, tab-separated: account, mode, rank, kept|retire, profile, xp, play seconds,
/// newest save (Unix ms), created (Unix ms); `-` for a missing value.
fn line(r: &accounts::DuplicateRow) -> String {
    let opt = |v: Option<i64>| v.map_or("-".to_owned(), |v| v.to_string());
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        r.account_id,
        r.mode,
        r.rank,
        if r.rank == 1 { "keep" } else { "retire" },
        r.profile_id,
        opt(r.xp),
        opt(r.play_seconds),
        opt(r.saved_at.map(|t| t.millis())),
        r.created_at.millis(),
    )
}

async fn duplicates() -> Outcome {
    let config =
        DbConfig::from_env_for("DATABASE_URL", "fi-cli-profiles-duplicates", Workload::Job)?;
    let db = fi_db::connect_lazy(&config)?;
    let rows = accounts::duplicate_profiles(&db).await?;
    db.close().await;
    println!("account\tmode\trank\taction\tprofile\txp\tplay_seconds\tsaved_at_ms\tcreated_at_ms");
    for r in &rows {
        println!("{}", line(r));
    }
    let accounts: std::collections::BTreeSet<_> = rows
        .iter()
        .map(|r| (r.account_id, r.mode.as_str()))
        .collect();
    let retire = rows.iter().filter(|r| r.rank > 1).count();
    tracing::info!(
        groups = accounts.len(),
        retire,
        "profiles duplicates: report only, nothing written"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fi_domain::UnixMs;
    use uuid::Uuid;

    #[test]
    fn a_line_names_ids_and_numbers_only() {
        let row = accounts::DuplicateRow {
            account_id: Uuid::nil(),
            mode: "noob".into(),
            profile_id: Uuid::nil(),
            rank: 2,
            xp: Some(10),
            play_seconds: None,
            saved_at: None,
            created_at: UnixMs::from_millis(5),
        };
        assert_eq!(
            line(&row),
            format!("{0}\tnoob\t2\tretire\t{0}\t10\t-\t-\t5", Uuid::nil())
        );
    }
}
