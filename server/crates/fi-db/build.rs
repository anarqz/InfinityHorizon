//! `sqlx::migrate!` embeds `migrations/` at compile time, but cargo does not know the macro reads
//! that directory: without this, a migration added after the first build is silently missing from
//! an incremental build (readiness then refuses the database, or a test runs the old schema). Every
//! change under `migrations/` (a new, renamed or edited file) recompiles the crate.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
