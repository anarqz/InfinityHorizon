# fi-db

The only crate that talks to Postgres (ADR 0004): the sqlx pool with per-workload session limits
(`Workload::{Service, Job, Migrate}`), the embedded migrations and readiness by checksum, login
roles (SCRAM verifiers, credentials generations), `PgSaveStore` (the Postgres `fi_saves::SaveStore`)
with every write also as a function over the caller's transaction (`acquire_in`, `renew_in`,
`release_in`, `commit_in`), and the admission lock of a pool (`sessions::lock_pool`).

Identity plane (SER-1.1, ACCOUNTS.md §4-§9), every write a function over the caller's transaction:
`identity` (identity tickets, launcher authorization codes with PKCE and exact redirect, device
codes with polling interval, legacy site-key lookup and spending), `accounts` (account + identity +
first profile with a random free discriminator + empty save line, device binding under
`fi_entitlements::sharing`, `/me` with the entitlement read model or free, deletion request/cancel
with `profile.changed`), `tokens` (launcher refresh families with rotation under a row lock, reuse
detection and the CR-L4 replay rule, 90/30-day lifetimes by platform; portal session tokens, 30 days
sliding, 90 absolute), `limits` (shared fixed-window rate limits, idempotency records without
secrets). Secrets arrive as SHA-256 only (fi-auth mints them).

Platform v1 modules: `gate` (admission per game, tickets' kid/exp, `touch_seen` liveness, the
batched sweeper, the queue summary; ADR 0015), `challenge`, `notices`, `moderation` and `mcp`
(core; CORE_API.md, ADR 0018), `universe` and `matchmaking` (saves-mm; ADR 0016, 0017),
`gameplane` (the proxied game requests of SER-M3: logon state, online profile and tables,
transaction replies, Xbox Live presence; GAME_PLATFORM.md §3a).

## Migrations (`migrations/`)

| File | Area |
| --- | --- |
| `20261017000100_foundation.sql` | Group roles `fi_runtime`, `fi_maintenance`; `fi_touch_row`, `fi_forbid_update` |
| `20261017000200_accounts.sql` | `game`, `account`, `account_site_key` (HMAC; wrapped Beta hashes), `profile` (+ `profile_xuid_seq`), `device_binding`, `refresh_token`, `legacy_device_token`, `account_audit`, `rate_limit`, `idempotency_record` |
| `20261017000300_play_sessions.sql` | `pool`, `play_session` (`max_until`, `game_until`) |
| `20261017000400_saves.sql` | `save_blob` (GC mark), `save_parked`, `save_revision` (play time, `parked_from`, kind/actor check), `save_head` (reset revision, play time, guard trigger), `save_event`, `integrity_strike` |
| `20261017000500_events.sql` | `outbox_event`, `progression_event`, `profile_progress` (`source_revision`), `session_summary`, `processed_event` (`outbox_event` and `processed_event` unused since CR-S55; dropped by a later release) |
| `20261017210828_identity.sql` | Identity Platform sign-in (ACCOUNTS.md §4.1): `account_identity` (uid ↔ account), `identity_ticket`, `launcher_auth_code` (PKCE, loopback redirect), `device_authorization` (device code, user code); `account_site_key.spent_at`, `refresh_token.last_used_at`, revoke reasons `replayed` and `deletion_requested`, idempotency scope `identity:<hmac>` |
| `20261018120000_billing_entitlements.sql` | Monetization (MON-1.2; written only by `services/billing`): `entitlement` (cached state, `access_until` bounded when VIP), `entitlement_event` (append-only ledger, `event_id UNIQUE`, `seq`), `admin_action` (append-only audit) |
| `20261019000100_gate.sql` | Platform v1, gate (ADR 0015, 0902): `game.maintenance_until`; `pool.game` (+ `UNIQUE (name, game)`); `play_session.game` (same-game trigger, composite FK to the pool), `last_seen_at`, `lease_ttl_ms` (60-minute ceiling), `save_line`, `ticket_kid`/`ticket_exp`; the fh2 `matchmaking` pool; game-first queue and sweeper indexes |
| `20261019000200_core.sql` | Platform v1, core (CORE_API.md, ADR 0018): `account.role` gains `admin`, `banned_until`; game switches (`maintenance_message`, `min_hook`, `min_launcher`, `accepting`); `notice`; `challenge_use`; social (`profile.social_*`, `social_comment`, `report`); `integrity_strike.game` and manual strikes; `admin_action` account actors; `mcp_client`, `mcp_request`, `mcp_connection`, `mcp_refresh` |
| `20261019000300_universe.sql` | Platform v1, saves-mm (ADR 0016, 0017): `board_entry` (best-first `order_key`), `universe_refusal` (CR-L14 once per line), `ghost_blob`, `ghost`, `player_position` (grid `cell`), `world`, `world_ban`, `mm_grant`, `mm_queue`, `mm_free_slot`, `gold_snapshot`; `game_presence.game`; `progression_event.kind` gains `ghost` |
| `20261019000400_gameplane.sql` | Platform v1, SER-M3 steps 3-4 (GAME_PLATFORM.md §3a; `gameplane`): `game_logon` (career level, badge, title, the tables checkpoint; never the game's IP, MAC or machine id), `game_profile` (UpdateAll, sections merged), `game_profile_table` (UpdateAllTables rows by key), `game_transaction` (replies kept for Reconcile, a ring of 8 slots per profile), `game_player_presence` (Xbox Live state and rich presence) |

Rules: SQL-first, `<UTC timestamp>_<area>.sql`, sorting after every file on main
(`scripts/check-migration-order.sh [base-ref]`, a CI step); `build.rs` recompiles the crate when a file
is added (sqlx embeds the folder at compile time); forward-only once production holds data
(SER-M6); sqlx refuses an edited applied migration; a first line `-- no-transaction` runs a file
outside a transaction (`CREATE INDEX CONCURRENTLY`). Other initiatives add their own files here
(monetization: `entitlement`, `entitlement_event`, `billing_*`, its ADR 0305 §3).

## Roles

OD-82 adds online_control: runtime has SELECT only; fi_maintenance can update the singleton.
Updates serialize through one transaction, increment the revision exactly once and append to
the bounded last-256-revision audit. Close is idempotent; budget reason preserves first closedAt.
Reopen requires the exact observed revision. The global policy never owns queue/seat state.
Use fi-cli online-control; see docs/contracts/ONLINE_CONTROL.md and migration
20261021060000_online_control.sql.

`fi_migrator` owns everything; `fi-cli migrate` creates `fi_app` (`fi_runtime`) and `fi_jobs`
(`fi_runtime`, `fi_maintenance`) from `FI_APP_PASSWORD` / `FI_JOBS_PASSWORD`, sending only SCRAM
verifiers computed client-side (the password never reaches the server, its logs or its errors).
With `FI_LOGIN_GENERATION=N` (production) they are `fi_app_gN` / `fi_jobs_gN`, and generations older
than N-1 are dropped (rotation runbook: docs/server/OBSERVABILITY.md §7).

## Tests

```sh
cargo test -p fi-db                                   # unit (migrations embedded in order, role names)
docker compose up -d --wait postgres
FI_TEST_DATABASE_URL=postgres://fi_migrator:fi_migrator_local@127.0.0.1:5452/forza_infinity_test \
  cargo test -p fi-db -- --include-ignored            # + the Postgres suites (pg_saves, pg_identity), every runtime path as a real fi_runtime login
docker compose down
```

CR-S55 (owner 2026-10-26, no outbox): no code writes `outbox_event` or reads `processed_event`; both stay
until every running image is this release's, then a later release drops them.
`20261026210000_progression_event_kind.sql` adds `progression_event (profile_id, kind)` for the
synchronous recount.

OD-74 / CR-S30: `20261020140000_native_signin.sql` drops only `launcher_auth_code` and
`device_authorization`. Identity mappings, identity tickets, sessions and profiles remain.
