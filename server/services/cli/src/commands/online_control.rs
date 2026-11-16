//! OD-82 online spending/operator stop. No billing-provider or cloud calls; DATABASE_URL only.
use super::Outcome;
use fi_db::{
    DbConfig, Workload,
    online_control::{self, CloseReason},
};
use fi_protocol::online_control::{OnlineState, OnlineStatus};

/// Usage lines appended to fi-cli help.
pub const USAGE: &str =
    "  online-control status | close --reason budget|operator | reopen --expected-revision <N>
                   global online stop (DATABASE_URL; mutations require fi_maintenance)";
const ARGS: &str = "usage: online-control status | close --reason budget|operator | reopen --expected-revision <nonnegative revision>";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Status,
    Close(CloseReason),
    Reopen(i64),
}
fn parse(args: &[String]) -> Result<Command, &'static str> {
    match args {
        [action] if action == "status" => Ok(Command::Status),
        [action, flag, reason] if action == "close" && flag == "--reason" => {
            reason.parse().map(Command::Close).map_err(|_| ARGS)
        }
        [action, flag, revision] if action == "reopen" && flag == "--expected-revision" => revision
            .parse::<i64>()
            .ok()
            .filter(|r| *r >= 0)
            .map(Command::Reopen)
            .ok_or(ARGS),
        _ => Err(ARGS),
    }
}
/// Runs only online-control; unknown commands are left to another module.
pub async fn run(command: &str, args: &[String]) -> Option<Outcome> {
    if command != "online-control" {
        return None;
    }
    Some(execute(args).await)
}
async fn execute(args: &[String]) -> Outcome {
    let command = parse(args)?;
    let config = DbConfig::from_env_for("DATABASE_URL", "fi-cli-online-control", Workload::Job)?;
    let pool = fi_db::connect_lazy(&config)?;
    let result = match command {
        Command::Status => online_control::read(&pool).await,
        Command::Close(reason) => online_control::close(&pool, reason).await,
        Command::Reopen(expected) => online_control::reopen(&pool, expected).await,
    };
    pool.close().await;
    let value = result?;
    let status = OnlineStatus {
        state: if value.open {
            OnlineState::Open
        } else {
            OnlineState::Closed
        },
        revision: value.revision.try_into()?,
        reason: value.reason.map(|r| match r {
            CloseReason::Budget => fi_protocol::online_control::CloseReason::Budget,
            CloseReason::Operator => fi_protocol::online_control::CloseReason::Operator,
        }),
        closed_at: value
            .closed_at
            .map(|at| {
                fi_protocol::instant::format_unix_ms(at.millis())
                    .ok_or("online control timestamp out of range")
            })
            .transpose()?,
        retry_after_seconds: if value.open { 5 } else { 300 },
    };
    println!("{}", serde_json::to_string(&status)?);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).into()).collect()
    }
    #[test]
    fn only_explicit_bounded_operations_parse() {
        assert_eq!(parse(&args(&["status"])), Ok(Command::Status));
        assert_eq!(
            parse(&args(&["close", "--reason", "budget"])),
            Ok(Command::Close(CloseReason::Budget))
        );
        assert_eq!(
            parse(&args(&["close", "--reason", "operator"])),
            Ok(Command::Close(CloseReason::Operator))
        );
        assert_eq!(
            parse(&args(&["reopen", "--expected-revision", "0"])),
            Ok(Command::Reopen(0))
        );
        assert_eq!(
            parse(&args(&[
                "reopen",
                "--expected-revision",
                "9223372036854775807"
            ])),
            Ok(Command::Reopen(i64::MAX))
        );
        for invalid in [
            vec![],
            vec!["status", "--force"],
            vec!["close"],
            vec!["close", "--reason", "other"],
            vec!["reopen"],
            vec!["reopen", "--expected-revision", "-1"],
            vec!["reopen", "--expected-revision", "9223372036854775808"],
            vec!["reopen", "--expected-revision", "1", "--force"],
        ] {
            assert_eq!(parse(&args(&invalid)), Err(ARGS));
        }
    }
    #[tokio::test]
    async fn invalid_arguments_fail_before_any_database_access() {
        assert!(run("other-command", &[]).await.is_none());
        assert_eq!(
            run("online-control", &[])
                .await
                .unwrap()
                .unwrap_err()
                .to_string(),
            ARGS
        );
    }
}
