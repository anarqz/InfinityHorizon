# fi-cli (services/cli)

One binary for every Cloud Run job and the compose `init` service (ADR 0002 §3). Jobs run one fixed
command each with `max_retries = 0`.

| Command | Job | Status |
| --- | --- | --- |
| `migrate` | `fi-migrate` | Done: applies migrations (`DATABASE_MIGRATOR_URL`), creates or updates `fi_app` and `fi_jobs` from `FI_APP_PASSWORD` / `FI_JOBS_PASSWORD` |
| `db-check` | `fi-db-check` | Done: fails unless every migration of this build is applied (`DATABASE_URL`) |
| `outbox-sweep`, `outbox reset-attempts` | `fi-outbox-sweep` (every 5 min) | SER-M5 |
| `consolidate-nightly` | `fi-consolidate-nightly` | SER-M5 |
| `saves-retention` | `fi-saves-retention` (nightly 04:00; `schedule` daily) | Done (2026-10-26): the save files of the newest 10 revisions per line plus the always-kept set stay, the others' bundles are pruned (rows stay); expired parked bundles nothing names are deleted (`fi_db::save_retention::prune`, SAVES_API.md §8; `DATABASE_URL`, `fi_maintenance`) |
| `saves-gc` | `fi-saves-gc` (nightly 04:30; `schedule` daily) | Done (2026-10-26): marks blob rows nothing unpruned references, sweeps those marked over 24 h ago (object, then the row or `object_deleted_at`; CLOUD_SAVES.md §6; `DATABASE_URL`, `FI_BLOB_*` or `CLOUD_SAVES_BUCKET`) |
| `saves-verify`, `saves restore`, `saves-export` | operator | SER-M2 |
| `account-erasure` | daily | SER-M1 |
| `beta-import` | operator | SER-M6 |
| `billing ...`, `entitlement ...` | monetization's (its ADR 0305 §9) | MON-M1 |

```sh
DATABASE_MIGRATOR_URL=postgres://fi_migrator:fi_migrator_local@127.0.0.1:5452/forza_infinity \
FI_APP_PASSWORD=fi_app_local_password FI_JOBS_PASSWORD=fi_jobs_local_password cargo run -p fi-cli -- migrate
```
