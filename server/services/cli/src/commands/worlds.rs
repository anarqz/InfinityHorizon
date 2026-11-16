//! The official Worlds' safety-ceiling alert (OD-209, OD-218, CR-S100; WORLDS_OFFICIAL.md §13):
//!
//! - `worlds-ceiling`: for every Worlds VM an agent has `PUT` a world of (`official_vm`, written by
//!   matchmaking with the VM's `FI_WORLDS` `maxWorlds`), counts its open official worlds; past 80%
//!   of the ceiling ([`fi_db::matchmaking::CEILING_ALERT_PERCENT`]) it mails the owner once, and
//!   once more when the VM is back at or under 80% (the recovery). The state is the row's
//!   `alerted_at`, written after the mail went out, so a failed send is retried on the next pass
//!   and a restart never repeats a sent one. `schedule` runs it every minute.
//!
//! The channel is OD-203's: mail through the jobs' SMTP settings (`FI_MAIL_BACKEND=smtp`,
//! `FI_SMTP_HOST`, `FI_SMTP_PORT`, `FI_SMTP_USER`, `FI_SMTP_PASSWORD` from the environment or
//! `FI_KEYS_DIR`, `FI_MAIL_FROM`) to `FI_ALERT_MAIL_TO`. Without that address or with
//! `FI_MAIL_BACKEND=log` the alert is a `warn` log line only, and the state still moves, so the
//! log carries one line per crossing. The mail holds the `vmId` and numbers, never player data.

use super::Outcome;
use fi_db::matchmaking::{self as mm, OfficialCeiling};
use fi_db::{DbConfig, Workload};
use fi_mail::{Mailer, Smtp, SmtpConfig};
use fi_runtime::config;

/// Usage lines appended to `fi-cli help`.
pub const USAGE: &str =
    "  worlds-ceiling   mail the owner when a Worlds VM's open official worlds pass 80% of its
                   maxWorlds safety ceiling, and when they are back under (CR-S100; DATABASE_URL,
                   a fi_maintenance login; FI_ALERT_MAIL_TO, FI_MAIL_BACKEND=smtp and FI_SMTP_*)";

/// Runs `command` with `args`; `None` when it is not a Worlds command.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    match command {
        "worlds-ceiling" => Some(worlds_ceiling(args).await),
        _ => None,
    }
}

/// What a pass does for one VM.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Nothing changed.
    Keep,
    /// It passed 80%: alert.
    Alert,
    /// It is back at or under 80%: recovery.
    Recover,
}

fn step(c: &OfficialCeiling) -> Step {
    match (c.over(), c.alerted) {
        (true, false) => Step::Alert,
        (false, true) => Step::Recover,
        _ => Step::Keep,
    }
}

fn message(c: &OfficialCeiling, alert: bool) -> (String, String) {
    if alert {
        (
            format!("Worlds VM {} passed 80% of its ceiling", c.vm_id),
            format!(
                "Worlds VM {vm} has {open} open official worlds of its maxWorlds {max} \
                 (over {pct}%).\n\nThe ceiling is a safety limit (CR-S100): at {max} the agent \
                 cannot open more, and a player under the 100-player limit would wait and then \
                 get \"service unavailable\". Check the VM's memory and CPU, then raise \
                 maxWorlds (FI_WORLDS and the agent's --max-worlds together) or add a VM.\n",
                vm = c.vm_id,
                open = c.open,
                max = c.max_worlds,
                pct = mm::CEILING_ALERT_PERCENT,
            ),
        )
    } else {
        (
            format!("Worlds VM {} back under 80% of its ceiling", c.vm_id),
            format!(
                "Worlds VM {vm} is back to {open} open official worlds of its maxWorlds {max}.\n",
                vm = c.vm_id,
                open = c.open,
                max = c.max_worlds,
            ),
        )
    }
}

/// The mailer and recipient of the alert, `None` when the alert goes to the log only.
fn mailer() -> Result<Option<(Smtp, String)>, Box<dyn std::error::Error>> {
    let Some(to) = config::optional("FI_ALERT_MAIL_TO").filter(|t| !t.trim().is_empty()) else {
        return Ok(None);
    };
    if config::optional("FI_MAIL_BACKEND")
        .as_deref()
        .map(str::trim)
        != Some("smtp")
    {
        return Ok(None);
    }
    let port = match config::optional("FI_SMTP_PORT") {
        None => 465,
        Some(p) => p
            .trim()
            .parse::<u16>()
            .map_err(|_| "FI_SMTP_PORT: a port number")?,
    };
    if let Some(product) = config::optional("FI_MAIL_PRODUCT") {
        fi_mail::set_product(&product).map_err(|_| "FI_MAIL_PRODUCT: 1-48 characters")?;
    }
    let smtp = Smtp::new(&SmtpConfig {
        host: config::optional("FI_SMTP_HOST").unwrap_or_else(|| "smtp.resend.com".into()),
        port,
        user: config::optional("FI_SMTP_USER").unwrap_or_else(|| "resend".into()),
        password: config::secret("FI_SMTP_PASSWORD")
            .map(|p| p.trim().to_owned())
            .unwrap_or_default(),
        from: config::optional("FI_MAIL_FROM").unwrap_or_default(),
    })?;
    Ok(Some((smtp, to.trim().to_owned())))
}

async fn worlds_ceiling(args: &[String]) -> Outcome {
    if !args.is_empty() {
        return Err("usage: worlds-ceiling".into());
    }
    let mail = mailer()?;
    let db = DbConfig::from_env_for("DATABASE_URL", "fi-cli-worlds-ceiling", Workload::Job)?;
    let pool = fi_db::connect_lazy(&db)?;
    let result = pass(&pool, mail.as_ref()).await;
    pool.close().await;
    result
}

async fn pass(pool: &sqlx::PgPool, mail: Option<&(Smtp, String)>) -> Outcome {
    // One read, then each send with no connection held (no transaction across a network call).
    for c in mm::official_ceilings(pool).await? {
        let alert = match step(&c) {
            Step::Keep => continue,
            Step::Alert => true,
            Step::Recover => false,
        };
        let (subject, text) = message(&c, alert);
        tracing::warn!(
            event = "worlds_ceiling",
            vm_id = c.vm_id.as_str(),
            open = c.open,
            max_worlds = c.max_worlds,
            alert,
            "{subject}"
        );
        if let Some((smtp, to)) = mail
            && let Err(e) = smtp.send(fi_mail::alert_mail(to, &subject, &text)).await
        {
            // Retried on the next pass: the state did not move.
            tracing::error!(event = "worlds_ceiling", vm_id = c.vm_id.as_str(), error = %e,
                "the alert mail was not sent");
            continue;
        }
        mm::official_ceiling_mark(pool, &c.vm_id, alert, c.open).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(open: i64, max: i64, alerted: bool) -> OfficialCeiling {
        OfficialCeiling {
            vm_id: "sao-1".into(),
            max_worlds: max,
            open,
            alerted,
        }
    }

    #[test]
    fn it_alerts_past_eighty_percent_once_and_recovers_once() {
        assert_eq!(step(&vm(6, 8, false)), Step::Keep, "75%");
        assert_eq!(step(&vm(7, 8, false)), Step::Alert, "87.5%");
        assert_eq!(step(&vm(8, 8, true)), Step::Keep, "already alerted");
        assert_eq!(step(&vm(7, 8, true)), Step::Keep);
        assert_eq!(step(&vm(6, 8, true)), Step::Recover);
        assert_eq!(step(&vm(52, 64, false)), Step::Alert, "81.25%");
        assert_eq!(step(&vm(51, 64, false)), Step::Keep, "79.7%");
        assert_eq!(
            step(&vm(4, 5, false)),
            Step::Keep,
            "exactly 80% is not past it"
        );
    }

    #[test]
    fn the_mail_names_the_vm_and_numbers_only() {
        let (subject, text) = message(&vm(7, 8, false), true);
        assert!(subject.contains("sao-1") && subject.contains("80%"));
        assert!(text.contains("7 open official worlds of its maxWorlds 8"));
        let (subject, text) = message(&vm(5, 8, true), false);
        assert!(subject.contains("back under"));
        assert!(text.contains("5 open"));
    }

    #[test]
    fn it_is_a_scheduled_command() {
        assert!(USAGE.contains("worlds-ceiling"));
        assert!(
            super::super::schedule::JOBS
                .iter()
                .any(|(n, every, _)| *n == "worlds-ceiling" && *every == 60)
        );
    }
}
