# fi-saves

Cloud saves v2 core (ADR 0005, [CLOUD_SAVES.md](../../docs/server/CLOUD_SAVES.md),
[SAVES_API.md](../../docs/contracts/SAVES_API.md)). No I/O and no runtime dependency: the launcher
(`crates/fi-launcher-core`, `x86_64-pc-windows-gnu`) uses the same bundle, digest and decision code
as the server. `cargo check --target x86_64-pc-windows-gnu` passes; linking a Windows binary needs
MinGW-w64 or zig, because `getrandom` (through `uuid`) links with raw-dylib (launcher ADR 0100 §4).

| Module | What |
| --- | --- |
| `lease` | Pure decisions: acquire (fencing token `fence_seq + 1`, takeover, same-device replacement, resume of a lapsed lease nobody took), renew, release, commit check (session + token + live + CAS on the head + kind/actor + mode rules + reset rule for restores + Hardcore gate binding), play-time credit |
| `store` | `SaveStore` trait: every write decided and written in one critical section; replay lookup, parked listing |
| `memory` | `MemoryCore` (sync, loom-switchable mutex) and `MemorySaveStore` (async, with a `Clock`); lines keep their mode |
| `blob` | `BlobStore` (content-addressed, create-only, shared objects) and `MemoryBlobStore` |
| `bundle` | FH2SAVE1 format ported from Beta: shape, LZ (method 2) and XPRESS Huffman (method 1) decoding, `commit` line checks, `ssmc`/SQLite and XML checks, content digest, ProfileData numbers, `pack`, `set_profile_int`, `commit_with`; `verify` (the server's single-pass check, one file in memory at a time, same answers as `validate`) |
| `gates` | Hardcore allowances on play time; missing base numbers count as zero; loud refusals |
| `reconcile` | The launcher's decision table: in sync, download, fast-forward, upload, first upload, conflict |
| `service` | `SaveService` (generic over a `Verifier`): upload (no commit off `If-Match`, lease precheck, verify, gate on play time against the newest readable base, pre-reset check, park, put blob, commit bound to the gated head, re-put), restore, reset, accept parked, conflict-loser parking, verified download |
| `conformance` (feature `testkit`) | The 28 scenarios every store must pass, concurrent ones and the gate race through the real service included |

Postgres implementation: `crates/fi-db/src/saves.rs` (same decisions, `save_head` row lock).
Fixtures: none on disk. `synthetic` (always built, no I/O) generates made-up saves in the FH2SAVE1
shape from a seed; real saves never enter the repository (AGENTS.md "Absolute rules"). The opt-in
real-save test reads them at run time: `FI_REAL_SAVES_DIR=<private dir> cargo test -p fi-saves
--test bundles -- --ignored`.

```sh
cargo test -p fi-saves --features testkit                          # 60 tests (33 unit, 7 fixtures, property, conformance, 18 service)
RUSTFLAGS="--cfg fi_loom" cargo test -p fi-saves --test loom --release   # loom model checks
```
