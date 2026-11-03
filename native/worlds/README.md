# worlds

Infinity Worlds: one Rust program that hosts online worlds (formerly `dedicated-server`; contract
DEDICATED_SERVER.md, WORLDS_OFFICIAL.md). One binary, one role per process: `worlds serve` is the
world engine described here; `worlds relay` and `worlds agent` are named but not implemented yet
(exit 7). The launcher runs it as a
child process for a private world (one world per process, its own UDP port); a Worlds host VM runs it
as one long-lived process for official (public) worlds: several worlds on one public UDP socket
(`--bind`), players connecting directly, no relay (OD-189). Both are
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
  and owned by that module. `src/fh2` is Forza Horizon 2: the NerdCore codec (`codec`: written for
  Infinity, OD-187), the side
  channels (`carstate`), the engines and `RelaySettings`, voice, the anti-cheat watch and the
  game-data catalog.

## Build

Rust 1.99 or newer (edition 2024). Exact pins, no C code: `serde`/`serde_json` (the control
channel, the strict grant claims) and the platform's own crypto crates at its versions:
`ed25519-dalek` (relay-grant signatures), `hmac` + `sha2` (FIUP's HMAC-SHA256), `getrandom` (the
FIUP challenge nonce).

```sh
cargo build --release                                    # Linux (the VM: add --target x86_64-unknown-linux-musl)
cargo build --release --target x86_64-pc-windows-msvc    # Windows (the launcher's build; MSVC linker)
cargo build --release --target x86_64-pc-windows-gnu     # Windows, from Linux (needs a MinGW or zig linker)
```

Set `DS_COMMIT=<40 lowercase hex>` when building to stamp the commit that `--version` and
`hello.build.commit` report (40 zeros otherwise). On `x86_64-pc-windows-msvc`, `build.rs` writes the
executable's VERSIONINFO (`FileDescription` "Infinity Worlds", `OriginalFilename` `worlds.exe`, contract §2.4) as a compiled
`.res` that the MSVC linker takes directly: no resource compiler and no extra dependency.
`DS_PRODUCT_VERSION` (the launcher's version, e.g. `2.0.5`) sets its product and file versions
(the crate version otherwise). Other targets carry no VERSIONINFO.

## Run

```text
worlds serve --profile private|public --control <endpoint> [options]
worlds serve --version          # also: worlds --version
worlds serve --health --control unix:<path>
worlds serve --probe-bind <port>
worlds relay | worlds agent     # not implemented yet: exit 7
```

| Option | Environment | Default |
| --- | --- | --- |
| `--profile` | `FI_DS_PROFILE` | required |
| `--control` | `FI_DS_CONTROL` | required: `unix:<absolute path>` (Linux) or `pipe:\\.\pipe\fi-ds-<32 hex>` (Windows) |
| (none) | `FI_DS_CONTROL_SECRET` | required with `pipe:`; read once, then removed from the environment |
| `--game-db` | `FI_DS_GAME_DB` | none; deprecated and ignored (the catalogs come by `catalog.set` / `catalog.parts`) |
| `--grant-keys` | `FI_DS_GRANT_VERIFY_KEYS` | none (`keys.set` replaces it) |
| `--bind` | `FI_DS_BIND` | public: required, the one public UDP socket `addr:port` (`[::]:port` dual-stack); private: refused |
| `--max-worlds` | `FI_DS_MAX_WORLDS` | private 1, public 4 (1-8) |
| `--max-seats` | `FI_DS_MAX_SEATS` | private 12, public 48 (1-96) |
| `--memory-limit-mb` | `FI_DS_MEMORY_LIMIT_MB` | private 0 (off), public 400 (at least 128) |
| `--drain-seconds` | `FI_DS_DRAIN_SECONDS` | 25 (0-300) |
| `--stats-seconds` | `FI_DS_STATS_SECONDS` | 60 (0 or 10-3600) |
| `--log-level` | `FI_DS_LOG_LEVEL` | `info` (`debug` refused in the public profile) |

Exit codes: 0 clean stop, 2 invalid configuration, 3 control endpoint unavailable, 4 control
authentication failed, 5 no common protocol version, 6 public socket or budget not set up,
7 role not implemented yet (`worlds relay`, `worlds agent`), 70 internal fault outside any world. `--probe-bind`: 0 bound, 3 in use, 4 denied, 1 other.

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
- **`world.open`** takes the tagged host (OD-186 item 7): the private profile only `{kind: "player", xuid, isPlayer}`,
  the public profile only `{kind: "official", vmId}` (`vmId` `[a-z0-9-]{1,32}`; grants name it `host: {official:
  vmId}`, the FIUP header the official host tag, WORLDS_OFFICIAL.md §3). The public profile forces `requireProof`. `hello.proto` is an integer (the step-1 range `[lo, hi]` is still read).
  `world.context` carries the context only and answers `{leaseUntilMs}`; `world.admission` carries the punches only.
- **Panics:** a panic inside a world's command, datagram or tick closes only that world
  (`world.closed {reason: "fault"}`); the panic hook only logs (anonymised); a panic on the loop
  thread is exit 70.
- **Feature `test-hooks`** adds a `debug.fault` op that panics inside a world on purpose, for the
  isolation checks; it is never enabled in a release build.

## PORT_STATUS

Step 1 was the process and its scaffold. Step 2 ported the free-roam engine (`src/fh2/engine/`)
from the Infinity C# world engine's behaviour, judged by WINDOWS' D2 golden vectors at the engine
level. Step 3 adds admission (`core/admission.rs`, `grant.rs`, `fiup.rs`), the relay legs
(`core/legs.rs`), the private world's network (NAT, UPnP, granted punches) and free-roam events
(`fh2/engine/races.rs`, `options.rs`): a world serves granted players.
The game features come next, each one passing its golden vectors recorded from the current C#
engine, in this recommended order:

| # | Feature | Where | Notes |
| --- | --- | --- | --- |
| 0 | NerdCore codec (`src/fh2/codec/`) | fh2 | core written for Infinity from NERDCORE_WIRE.md, the C# engine and fh2-mods as references (OD-187; no code copied): bit stream, header, reliability state, objects, synced variables, every message body but 22 (none exists); every §7 variable list (7.1-7.12), checked against the corpus's object payloads (**done**) |
| 1 | Free roam core: NerdCore handshake and refusals 1-4, acks and resends, keepalive, TimeSync, object history with `HistoryMaxEntries` (trim, then drop; 16384), queue and rate caps, quiet drop, takeover; quiet joins (T25), weather, group navigation, held opens, the car-state side channel | fh2 | **done** for free roam: the D2 Delivery, Boundary, WorldFoundation, Integration and SideChannel suites and the free-roam RelayEngineTests replay exactly |
| 2 | Admission: relay-grant verification (Ed25519), FIUP v1 proof host, held opens, kicks and revocations, network hints, the private-address rule | core | **done** (private tests `admission.rs`) |
| 3 | Private network: STUN/NAT and UPnP wired to the world socket (lease, renewal, alternate port), punches, `world.network` | core | **done**: wired, granted punches (2 per candidate, 10 per 15 s per grant), the 5 s STUN keepalive between rounds (one IPv4 server in turn) |
| 4 | Relay legs (FIR1 HOST role) for private worlds; the public profile's one direct socket | core | **done**: private legs; public worlds share one public socket (`--bind`), a FIUP frame routed by its grant id, every other datagram by its proven endpoint; no legs, STUN, UPnP or punches (OD-189) |
| 5 | Side channels: car state, control/state, their authorisation and rates | fh2 | livery is retired and dropped |
| 6 | Engine routing by lobby type, car meet engines 3 and 6, moves between engines, weather, quiet joins, the 30 s seat behaviour | fh2 | **done** (`fh2/mod.rs`): each OpenConnection goes to its lobby's engine (made on first use), a seat follows its endpoint (AdmittedLate included), a public world runs its one `activity` (refusal 4 for the others) |
| 7 | Races: grid, start, scores, departures, deadlines; two players for everything; event progress HUD (43/44, start line); race-end split and finishers staying participants | fh2 | **done** (RaceLifecycle, GameOptions, EventProgress suites) |
| 8 | Event phasing and universes; tag modes (Infected, King) and teams | fh2 | **done** (`races.rs`, `tag.rs`, `teams.rs`) |
| 9 | Car meet invitations and the game-session request/answer | fh2 + core | **done** (`carmeet.rs`; `world.gameSession.request`/`answer` by `rid`) |
| 10 | Road Trip (vote, events, alone/wait/drop) | fh2 | **done** (`roadtrip.rs`): the catalog rows arrive by `catalog.set` (no game file is read); `RoadTripServerState` carries Solo = 3 |
| 11 | Bucket-list co-op: pairing, 61 gating and fallback, message 65's universe per pair | fh2 | **done** (`fh2/engine/bucket.rs`): the universe is BucketList1..6 (18..23), one per pair, at most 6 per world; 66-68 carry the receiver-relative keys first |
| 12 | Voice: off/open/proximity, blocks from both sources, lobby mutes, team voice, state down | fh2 | **done** (`fh2/voice.rs` codec, `fh2/engine/voice.rs`): the RelayVoice suite and every proximity, team and moderation vector replay exactly; team voice reads the teams (`teams.rs`) |
| 13 | The anti-cheat watch, the parts rule and the watch roster | fh2 | **done** (`fh2/engine/watch.rs`): the six watch vectors replay exactly; the parts rows come from the controller on demand (`catalog.need`/`catalog.parts`), and until a model's rows arrive the parts rule says nothing about it |
| 14 | `diag`, `peer.rejected`, the engine lifecycle line budget | core + fh2 | **done**: `diag` every 3 s (2048, one at close), `peer.rejected` (128 per 30 s, 1 s dedup), `peer.synced`/`peer.left`, `world.empty`/`world.occupied`, note lines at info |

## Decisions taken in step 2

- **The engine reads messages with its own acceptance set** (`engine/message.rs`): exactly the ids it
  handles or forwards; any other id ends the packet (a `truncated_packet` rejection), as the
  reference engine does. The codec's registry (the full §6 catalogue) is for decoding anything.
- **Acknowledgements** (`engine/datagram.rs` `AckWindow`, MACBOOK's ruling in step 3): the game's
  rule, the codec's `reliable::ReceiveState` (ids modulo 0xffff; an id equal to the newest is a
  duplicate; one 0x3fff or more away is dropped silently). The C# engine wraps at 2^16; no vector
  of the corpus crosses the wrap, so none disagrees today.

## Decisions taken in step 3

- **Size at ingest** (WINDOWS' review): a datagram over 1,372 bytes is refused on arrival; an
  object whose relayed form (relay key, the server variables a snapshot adds) or an options
  fragment whose grant/update copy cannot fit one datagram is refused before it takes a sequence
  or enters a history or the journal, so nothing is dropped for size after its sequence was used.
- **Signatures:** `verify_strict`, keys refused at `keys.set` when they do not decompress or are
  of small order; claims strict (no unknown or duplicate member, depth 4), verified on the exact
  bytes. A punch that does not have the wire's shape is skipped (fail closed for that punch).
- **Engine `now` and admission:** the engines ask with the world's monotonic ms; the gate turns it
  into the trusted clock (`max(serverTimeMs-derived, monotonic)`, never back).
- **`world.empty`/`world.occupied`:** transitions only (a new world starts empty, no event); the
  host's own game, synced or handshaking, is not an occupant.
- **Not yet:** engine routing by lobby type (only free roam exists, so `AdmittedLate` moves
  nothing).
- **Datagram limit (WINDOWS' rule, 2026-11-02):** packets split only between messages (at most 8
  messages, a 1,189-byte body); a single message over `(1372 - 11) × 8` bits is dropped, never
  sent (`send.oversize_dropped`); a late joiner's catch-up create folds updates only up to
  `10836 - 128` bits, so it always fits.
- **Map order:** the reference engine's maps enumerate in slot order (a freed slot is reused
  first); `engine/dict.rs` keeps that order, which the bytes of a snapshot or of a leaving
  player's deletes show.
- **Randomness** (`engine/random.rs`): one SplitMix64 stream per purpose (weather, tag, team,
  attempt ids, STUN transaction ids) and the seeded subtractive generator for the weather's draws.

## Decisions taken in the voice port

- **Unknown engine-level voice mode is off** (MACBOOK): the reference engine throws; the control
  channel stays strict (`world.voice` refuses any other mode), so only an engine caller can pass
  one (`VoiceMode::or_off`).
- **Voice blocks** (§6.4): one table per world (`VoiceBlockTable`, the C# `VoiceBlockTable`
  semantics) fed by both sources, shared with every engine's policy and compared with the wall
  clock (Unix seconds) at each question; a change calls `voice_blocks_changed` on every engine.
  `world.voiceBlocks` answers `changed` when a block changed (a source repeating itself changes
  nothing).
- **State downs of a control command** (`world.voice`, `world.voiceBlocks`) are queued in the
  engine and leave with its next receive or tick (at most 20 ms later): `GameWorld::command` has
  no datagram sink.

## Decisions taken in step 11 (bucket-list co-op)

- **66-68 keys** (NERDCORE_WIRE.md §6.3; the final C# `b43618a`): each status message starts with
  the receiver-relative PlayerState keys (7-bit ids; the receiver's own state is an own key, anyone
  else's a relay key): 66 the waiter's; 67 the joiner's who completed the pair, then the waiter's;
  68 the ender's (its 62, new request or leave), then its partner's; then the challenge (s32); 68
  ends with the ending 62's flag (false for a new request or a leave). 66 goes to every synced
  player but the waiter, 67 and 68 to every synced player. When a key's PlayerState is missing (a
  leaver's), nobody gets it: `bucket.status_skipped` notes the receivers once per message.
- **New note** `bucket.not_paired` `[waiter, requester, 6]`: two players would pair but all six
  bucket-list universes are in use (a guard; 12 players make at most 6 pairs).
- A pair's first car prediction after 61 is told from the one at 61 by value, not by identity.

## Decisions taken in the watch port

- **Parts rows on demand** (MACBOOK): the engine reads no game database. A model whose rows the
  catalog store does not have yet is not judged and asks for them (`catalog.need`); the car is
  checked again on the engine's tick once they come (the reference reads its database at once).
  `known: false` (no slots) is the reference's "model not in this game database".
- **`world.watch` rules** are read as `PeerWatchRules.From` and must pass `WatchRules.Valid`
  (else `bad_request {field: "watch.rules"}` / `"rules"`); they apply to every engine, made now or
  later. Each `watch.report` gets its `id` from the world's random source.
- **Note numbers** are written as the reference's invariant-culture custom formats: 15
  significant digits for a double (7 for a single), rounded half away from zero.

## Decisions taken in step 4

- **Reference:** the frozen C# engine at client `b43618a` and WINDOWS' corpus re-recorded from it
  (monorepo `f3c46f01`): all 342 replayable vectors pass, none pinned as a deviation.
- **Catalogs** (MACBOOK decision 1): the engine reads no game file. `catalog.set {game, version,
  roadTrip: {events}}` carries the Road Trip rows the reference reads (event id, location, race mode,
  the mode's minimum players, the allowed classes; ≤ 1024 events, latest-wins, idempotent);
  `catalog.parts` answers the parts rule's `catalog.need` per model (64-model LRU). A Road Trip
  OpenConnection with no catalog: `ds.world.no_roadtrip` once, and free roam takes it (private) or
  refusal 4 (a public Road Trip world). `--game-db` is accepted and ignored.
- **Keys** (decision 2): `keys.set` refuses a key that does not decode canonically, is of small
  order or lies outside the prime-order subgroup (`is_torsion_free`), as the reference's
  `ValidatePublicKeyFull`.
- **Packet ids** (decision 3): until its handshake completed a peer's id may not jump more than
  0x1000 forward (dropped, no ack, `receive.id_jump_dropped`, 4 notes per peer); a synced peer
  gets the game's window (a dropped or stale id is answered with the acknowledgement, as the
  reference answers it).
- **Corrections where no vector depends on the C#** (decision 4): `Clear()` also forgets the event
  and its owner; an unknown engine-level voice mode is off. The vectors settle ReleaseJoiner's
  journal targets and AssignTeams as the reference does them.
- **Public worlds:** `world.open.activity` (and `carMeetLobby`) is required in the public profile and
  refused in the private one; `lobbies` lists only that activity's lobby.

## Acknowledgements

The fh2-mods project's online research (https://github.com/conner-mcvay/fh2-mods, `tools/online`, by conner-mcvay and
contributors) was an inspiration and a reference for this work. No code from it is used here.

**References (owner decision OD-187, final rule).** This server was written for Infinity with these references: the
Infinity C# world engine (the launcher's former in-process WorldServer, client `b43618a`) for every behaviour, and its
golden vectors; our NERDCORE_WIRE.md for the game's network format; fh2-mods' code as inspiration and reference; RFC
8489/5389 (STUN) and the UPnP IGD WANIPConnection/SSDP specifications for NAT discovery and port mapping. No code is
copied from fh2-mods or from the retired Beta relay (`deprecated/native-relay`); the STUN, UPnP, NAT and car-state
modules were rewritten from those specifications for this server, and no new reverse engineering was done for it.
