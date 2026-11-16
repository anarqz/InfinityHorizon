//! The commands of the local environment v0 (docs/server/LOCAL_ENV.md), one module per
//! workstream so parallel work never edits the same file: [`platform`] (workstream A) and
//! [`nightly`] (workstream B's consolidation; its outbox, Pub/Sub and lake commands left with ADR
//! 0007, owner 2026-10-26); platform v1 adds [`gate`]; OD-67 adds [`audit`]; the hybrid Hardcore checks add [`integrity`]; HANDOFF §5 adds [`retention`]; OD-173 adds [`dashboard`] (the play dashboard's hourly snapshot). `main` tries them in that
//! order after its own commands; a module answers `None` for a command it does not know.

pub mod audit;
pub mod dashboard;
pub mod gate;
pub mod integrity;
pub mod nightly;
pub mod online_control;
pub mod platform;
pub mod profiles;
pub mod retention;
pub mod saves;
pub mod schedule;
pub mod worlds;

/// What a command returns.
pub type Outcome = Result<(), Box<dyn std::error::Error>>;
