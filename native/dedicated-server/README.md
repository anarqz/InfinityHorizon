# dedicated-server

The Infinity world server: one Rust program that hosts online worlds. The launcher runs it as a
child process for a private world (one world per process, its own UDP port); the relay VM runs it
as one long-lived process for public worlds (several worlds behind the co-located relay). Both are
the same build and the same code; a profile (`--profile private|public`) sets the limits.

It holds no disk state, writes nothing, ships no game data, and opens no listener besides its
world socket(s) and its control endpoint.

## Layout

- **Game-agnostic core** (`src/core`, `src/control`, `src/runtime.rs`, `src/net.rs`, `src/stun.rs`,
  `src/nat.rs`, `src/upnp.rs`, `src/log.rs`): the control channel, worlds, admission state, limits
  and the public memory budget, the timer wheel, events, the loop, sockets and logs. The core is
  deterministic and I/O-free (`core::Server`): time, randomness and socket binding are supplied
  by the caller, so the same API is driven by the process loop and by golden-vector replays.
- **One module per game** behind `core::game::{Game, GameWorld}`: `world.open` names the game
  (`"game": "fh2"`) and every game-scoped field (`races`, `voice`, `watch`, `settings`) is parsed
  and owned by that module. `src/fh2` is Forza Horizon 2: the NerdCore codec (`bits`, `nerd`), the
  side channels (`carstate`), the engines and `RelaySettings`, voice, the anti-cheat watch and the
  game-data catalog.

## Build

Rust 1.99 or newer (edition 2024). One dependency, `serde_json`, pinned exactly; no C code.

```sh
cargo build --release                                    # Linux (the VM: add --target x86_64-unknown-linux-musl)
cargo build --release --target x86_64-pc-windows-gnu     # Windows, from Linux (needs a MinGW or zig linker)
```

Set `DS_COMMIT=<40 lowercase hex>` when building to stamp the commit that `--version` and
`hello.build.commit` report (40 zeros otherwise). The Windows executable's VERSIONINFO
(`FileDescription` "Infinity world server") is added by the launcher's build.

## Run

```text
dedicated-server --profile private|public --control <endpoint> [options]
dedicated-server --version
dedicated-server --health --control unix:<path>
dedicated-server --probe-bind <port>
```

| Option | Environment | Default |
| --- | --- | --- |
| `--profile` | `FI_DS_PROFILE` | required |
| `--control` | `FI_DS_CONTROL` | required: `unix:<absolute path>` (Linux) or `pipe:\\.\pipe\fi-ds-<32 hex>` (Windows) |
| (none) | `FI_DS_CONTROL_SECRET` | required with `pipe:`; read once, then removed from the environment |
| `--game-db` | `FI_DS_GAME_DB` | none |
| `--grant-keys` | `FI_DS_GRANT_VERIFY_KEYS` | none (`keys.set` replaces it) |
| `--relay` / `--bind` | `FI_DS_RELAY` / `FI_DS_BIND` | `127.0.0.1:5101` / `127.0.0.1:0` (public only) |
| `--max-worlds` | `FI_DS_MAX_WORLDS` | private 1, public 4 (1-8) |
| `--max-seats` | `FI_DS_MAX_SEATS` | private 12, public 48 (1-96) |
| `--memory-limit-mb` | `FI_DS_MEMORY_LIMIT_MB` | private 0 (off), public 400 (at least 128) |
| `--drain-seconds` | `FI_DS_DRAIN_SECONDS` | 25 (0-300) |
| `--stats-seconds` | `FI_DS_STATS_SECONDS` | 60 (0 or 10-3600) |
| `--log-level` | `FI_DS_LOG_LEVEL` | `info` (`debug` refused in the public profile) |

Exit codes: 0 clean stop, 2 invalid configuration, 3 control endpoint unavailable, 4 control
authentication failed, 5 no common protocol version, 6 public socket or budget not set up,
70 internal fault outside any world. `--probe-bind`: 0 bound, 3 in use, 4 denied, 1 other.

Logs are JSON lines on stdout, written off the loop through a bounded queue that counts what it
drops. They never carry a player identity, an address, a key or game payload bytes.

The control channel speaks JSON lines, protocol version 1: `hello`, `keys.set`, `capacity`,
`health`, `stats`, `drain`, `shutdown`, and the `world.*` commands, with events back to the one
controller session. A Unix socket (directory `0700`, socket `0600`) on Linux; on Windows the
launcher creates the named pipe and the server connects to it (retrying for up to 5 s) and proves
it reached the right launcher with the per-launch secret.

## Decisions taken where the specification was silent (step 1)

- **Loop:** one loop thread owns every world; socket readers and control readers are plain
  threads that only feed one bounded channel, which the loop waits on with the next timer
  deadline as its timeout (no deadline: no wake-up). std has no readiness poll over a UDP socket,
  a control stream and a timer; a later step may swap the channel for `mio` without touching
  `core`.
- **Timer wheel:** deadlines coalesce into 10 ms slots; housekeeping runs once a second only
  while a world exists, engine ticks only for a world that is not idle.
- **Stats lines** are logged every `--stats-seconds` while worlds exist, and once when the last
  world closes (no timer with no worlds).
- **Unix endpoint:** the directory is created `0700` when missing; an existing directory that
  group or others can enter is refused (exit 3); a stale socket nobody answers on is replaced.
- **Pipe client:** retries opening the pipe (not created yet, busy) every 50 ms for 5 s instead of
  `WaitNamedPipe`, and asks for identification-only impersonation. One thread reads and writes
  (a synchronous pipe handle serialises its I/O), reading only what `PeekNamedPipe` reports.
- **Sessions:** a request from a non-controller connection other than `hello`, `health`,
  `capacity` or `stats` is `bad_request {field: "op", reason: "controller_only"}`. `unauthorized`
  and `proto_unsupported` end the process only for the pipe or a controller `hello`, never for a
  health probe.
- **Event `seq`** is assigned when an event leaves the bounded queue, so a drop is reported by
  `events.dropped` before the next event and the sequence the controller reads never has an
  unexplained gap; coalescing a newer `world.state` over an unsent one is not a drop.
- **Validation:** a malformed field is `bad_request` and changes nothing; a well-formed snapshot
  that fails the admission checks is `context_invalid` (or `no_keys`) and clears admission.
- **Lapse without a context:** a world that never received `world.context` counts its 45 s from
  `world.open` for the self-close of a controllerless public world.
- **`world.open` without `game`** means `"fh2"` (the only game before the field existed).
- **`world.open` in the public profile** forces `requireProof` and refuses `host.isPlayer: true`.
- **Panics:** a panic inside a world's command, datagram or tick closes only that world
  (`world.closed {reason: "fault"}`); the panic hook only logs (anonymised); a panic on the loop
  thread is exit 70.
- **Feature `test-hooks`** adds a `debug.fault` op that panics inside a world on purpose, for the
  isolation checks; it is never enabled in a release build.

## PORT_STATUS

Step 1 (this commit) is the process and its scaffold; the engines admit nobody yet (fail closed).
The game features come next, each one passing its golden vectors recorded from the current C#
engine, in this recommended order:

| # | Feature | Where | Notes |
| --- | --- | --- | --- |
| 1 | Free roam core: NerdCore handshake and refusals 1-4, acks and resends, keepalive, TimeSync, object history with `HistoryMaxEntries` (trim, then drop; 16384), queue and rate caps, quiet drop, takeover | fh2 | the 2026-10-20 relay and the C# `RelayEngine`, `.Join`, `.Packets` |
| 2 | Admission: relay-grant verification (Ed25519), FIUP v1 proof host, held opens, kicks and revocations, network hints, the private-address rule | core | needs an Ed25519 verifier (a pure-Rust dependency or a reviewed implementation) |
| 3 | Private network: STUN/NAT and UPnP wired to the world socket (lease, renewal, alternate port), punches, `world.network` | core | `nat.rs`/`upnp.rs` are here, sans-I/O, not wired yet |
| 4 | Relay legs (FIR1 HOST role) for private worlds, then the public socket multiplexed by grant id | core | the public socket drops everything until this lands |
| 5 | Side channels: car state, control/state, their authorisation and rates | fh2 | livery is retired and dropped |
| 6 | Engine routing by lobby type, car meet engines 3 and 6, moves between engines, weather, quiet joins, the 30 s seat behaviour | fh2 | |
| 7 | Races: grid, start, scores, departures, deadlines; two players for everything; event progress HUD (43/44, start line); race-end split and finishers staying participants | fh2 | |
| 8 | Event phasing and universes; tag modes (Infected, King) and teams | fh2 | |
| 9 | Car meet invitations and the game-session request/answer | fh2 + core | |
| 10 | Road Trip (vote, events, alone/wait/drop) with the catalog reader | fh2 | needs a read-only SQLite reader of the catalog; `RoadTripServerState` includes Solo = 3 |
| 11 | Bucket-list co-op: pairing, 61 gating and fallback, message 65's universe per pair | fh2 | the universe must be BucketList1..6 (18..23), one per pair, at most 6 per world |
| 12 | Voice: off/open/proximity, blocks from both sources, lobby mutes, team voice, state down | fh2 | |
| 13 | The anti-cheat watch, the parts rule and the watch roster | fh2 | the single detector on public worlds |
| 14 | `diag`, `peer.rejected`, the engine lifecycle line budget | core + fh2 | |

## Provenance

The game-protocol layer (NerdCore packets and the bit stream in `src/fh2/nerd.rs` and `src/fh2/bits.rs`, and the relay
basics the engine grew from) is derived in part from fh2-mods (https://github.com/conner-mcvay/fh2-mods), MIT,
Copyright (c) 2026 conner-mcvay and contributors. Its licence text is in
[`../../licenses/fh2-mods-LICENSE.txt`](../../licenses/fh2-mods-LICENSE.txt); the MIT notice applies to those portions,
and the crate as a whole is GPL-3.0-only like the rest of this repository.
