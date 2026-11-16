# fi-api (services/api)

The HTTP service of the account plane and the game plane (ADR 0002). One image, several Cloud Run
services that differ only in configuration:

| Deployment | `FI_SURFACES` | `FI_POOL` |
| --- | --- | --- |
| `fi-account` | `launcher,portal,internal` | (none) |
| `fi-game-vip` | `game` | `vip` |
| `fi-game-free` (later) | `game` | `free` |

Configuration: `DATABASE_URL` (required), `PORT` (8080), `DB_POOL_MAX`, `DB_ACQUIRE_TIMEOUT_MS`,
`DB_STATEMENT_TIMEOUT_MS`, `GCP_PROJECT_ID`, `RUST_LOG`/`LOG_LEVEL`, `LOG_FORMAT=pretty`.

Today (SER-M0): `/health/live`, `/health/ready` (Postgres answers and the schema is at this build's
migration), `/meta` (service, version, revision, project, surfaces, pool). Routes arrive per
milestone: `/launcher/v1` accounts and tokens (SER-M1), sessions and saves (SER-M2), the game surface
(SER-M3), admission (SER-M4), `/internal/v1` push endpoints (SER-M5). The `/portal/v1` handlers are
the portal initiative's, inside the `portal` surface (OPEN_QUESTIONS Q17).

```sh
cargo test -p fi-api
docker compose up -d --wait api && curl http://127.0.0.1:8090/meta
```
