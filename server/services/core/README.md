# fi-core

The core deployable of platform v1 (ADR 0013, [CORE_API.md](../../docs/contracts/CORE_API.md)):
fi-api's identity routes of `/launcher/v1` (not sessions, not saves), the web's `/portal/v1`
(cookie session, Turnstile), `/internal/v1`, `/core/v1` (status, notices, `/me`, social hub,
admin and moderation) and the MCP skeleton (`/mcp`, off by default). Settings: `src/config.rs`.

```sh
docker compose up -d --wait postgres auth-emulator
FI_TEST_DATABASE_URL=postgres://fi_migrator:fi_migrator_local@127.0.0.1:5452/forza_infinity_test \
FIREBASE_AUTH_EMULATOR_HOST=127.0.0.1:9399 cargo test -p fi-core -- --include-ignored
docker compose up -d --wait infinity   # locally fi-core's routes run in fi-platform's `infinity` service (OD-122)
```

## Progress log (lane core, PV1-C)

2026-10-19, compose project `fi-core` (postgres :5472, auth-emulator :9419, fake-gcs :4473,
pubsub :8316), `CARGO_TARGET_DIR` in the session scratchpad:

- `cargo fmt --all --check`: clean.
- `cargo clippy -p fi-core -p fi-api -p fi-db -p fi-runtime -p fi-auth -p fi-protocol --all-targets -- -D warnings`: clean.
- `cargo test -p fi-core -p fi-api -p fi-db -p fi-runtime -p fi-auth -p fi-protocol -- --include-ignored`:
  fi-core 20 (12 end to end over the whole router), fi-api 22 (its e2e included), fi-db 13 unit +
  pg_core 8, pg_events 2, pg_identity 10, pg_invariants 8, pg_saves 10, pg_sessions 9, fi-runtime 11,
  fi-auth 31, fi-protocol 18 + 3 parity: all green.
- `pnpm --filter @forza-infinity/contracts typecheck`: clean.
- `docker compose -f docker-compose.yml -f compose/core.yml config --quiet`: exit 0.
- Host smoke of the binary: `/health/ready` 200, `/core/v1/status` 200 `public, max-age=10`,
  `/mcp` 401 with the resource metadata pointer, `/launcher/v1/info` with `services` and `games`.
- Security review: docs/server/REVIEW_LOG.md, "Platform v1, core lane review".
