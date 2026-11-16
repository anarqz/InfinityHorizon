//! Postgres adapter for ForzaInfinity (ADR 0004). The only crate that talks to the database.
//!
//! - [`pool`]: connection settings (pool size within the Cloud SQL connection budget, statement and
//!   lock timeouts, `application_name` per service), lazy connect and the readiness ping.
//! - [`migrate`]: the embedded sqlx migrations (`migrations/`, forward-only once production holds
//!   data) and the login roles.
//! - [`saves`]: [`PgSaveStore`], the Postgres implementation of `fi_saves::SaveStore`, and each of
//!   its writes as a function over the caller's transaction (`acquire_in`, `commit_in`, ...).
//! - [`sessions`]: the admission lock of a pool, taken first in the admission transaction.
//! - [`identity`]: the Identity Platform exchange's credentials (identity tickets, launcher
//!   authorization codes, device codes) and the legacy site-key link.
//! - [`accounts`]: accounts, identities, profiles, devices (sharing limits), `/me`, deletion.
//! - [`credential`]: a `local` instance's e-mail and password credentials (OD-122 M2.3).
//! - [`tokens`]: refresh-token families (rotation, reuse detection, CR-L4 replay) and portal
//!   session tokens.
//! - [`limits`]: shared rate limits and idempotency records.
//! - [`liveries`]: the players' car liveries, stored for every ticket (CR-S59).
//!
//! Local environment v0 (docs/server/LOCAL_ENV.md §9; one module per workstream, filled in
//! parallel): [`admission`] and [`game`] (A), [`progression`] (B, the read models; the outbox and
//! the lake stand-in left with ADR 0007, owner 2026-10-26), [`invariants`] (C).
//!
//! Platform v1, lane saves-mm: [`universe`] (boards, ghosts, positions, the gold snapshot) and
//! [`matchmaking`] (worlds, grants, the free limit and queue); SER-M3: [`freeroam`] (the game's
//! MPSD sessions and SmartMatch tickets, the caller's world).
//!
//! Platform v1, lane gate: [`gate`] (admission per game, tickets' kid/exp, liveness, the batched
//! sweeper, the queue summary). Lane core: [`challenge`], [`notices`], [`moderation`], [`mcp`].
//!
//! SER-M3 (the game's proxied requests, GAME_PLATFORM.md §3a): [`gameplane`] (logon state, online
//! profile and tables, transaction replies, Xbox Live presence). SER-M3 step 6 (the Supporters'
//! universe, OD-48): [`visibility`] (who sees whom), [`scores`] (boards, ghosts, rivals, XP),
//! [`drivatars`], [`integrity`] (the RPC gates' baseline). SER-M3 step 7: [`friends`] (the friend
//! graph), [`inbox`] (the game's in-game inbox: messages, news, the rivals sync). OD-52: [`purge`]
//! (Hardcore → Noob: the warning, the purge, the retired save line), [`cloud`] (who keeps a save
//! line; the renewal mark). SER-M3 step 8: [`clubs`] (Car Clubs, one mode each, CR-S70). OD-60: [`seeds`] (the clean Hardcore snapshot, a line's revision 1).

pub mod accounts;
pub mod activity;
pub mod admission;
pub mod challenge;
pub mod cloud;
pub mod clubs;
pub mod console;
pub mod content;
pub mod crash;
pub mod credential;
pub mod dashboard;
pub mod discord;
pub mod drivatars;
pub mod erasure;
pub mod fm5;
pub mod freeroam;
pub mod friends;
pub mod game;
pub mod gameplane;
pub mod gate;
pub mod guard_nightly;
pub mod identity;
pub mod inbox;
pub mod integrity;
pub mod invariants;
pub mod launcher_policy;
pub mod limits;
pub mod liveries;
pub mod matchmaking;
pub mod mcp;
pub mod migrate;
pub mod moderation;
pub mod notices;
pub mod observations;
pub mod online_control;
pub mod pool;
pub mod progression;
pub mod purge;
pub mod registration;
pub mod relay_reports;
pub mod rename;
pub mod retention;
pub mod save_retention;
pub mod saves;
pub mod scores;
pub mod seed_profile;
pub mod seeds;
pub mod sessions;
pub mod storefront;
pub mod summary;
pub mod tokens;
pub mod universe;
pub mod visibility;
pub mod voice;

pub use pool::{DbConfig, DbError, Workload, connect_lazy, ping};
pub use saves::PgSaveStore;
pub use sqlx::PgPool;
