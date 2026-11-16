# fi-runtime

Plumbing shared by every backend service (ADR 0002 §4, ADR 0011):

- `config`: typed environment variables; a missing or malformed one fails startup.
- `telemetry`: logs as Cloud Logging structured JSON (`severity`, `message`, `timestamp`,
  `logging.googleapis.com/trace` and `spanId`), span fields flattened into each line;
  `LOG_FORMAT=pretty` locally.
- `http`: request context middleware (request id, W3C `traceparent` or `X-Cloud-Trace-Context`),
  access line (DEBUG when fast and successful, WARN or ERROR otherwise: ADR 0010 §3),
  `/health/live`, `/health/ready`, `/meta`, serving with graceful shutdown on SIGTERM.

```sh
cargo test -p fi-runtime   # 5 tests
```
