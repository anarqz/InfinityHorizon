# worlds

Infinity Worlds: one Rust program that hosts online worlds (formerly `dedicated-server`; contract
DEDICATED_SERVER.md, WORLDS_OFFICIAL.md). One binary, one role per process: `worlds serve` is the
world engine described here, `worlds agent` the world agent of a Worlds host (below; Linux), and
`worlds relay` is named but not implemented yet (exit 7). A Worlds host VM runs it as one
long-lived process for official worlds: several worlds on one public UDP socket (`--bind`),
players connecting directly, no relay (OD-189). There are no player-hosted worlds (OD-208): the
launcher runs no engine, and the engine has no private profile, no world socket of its own, no
STUN to a host, no UPnP, no punching and no relay legs (WORLDS_OFFICIAL.md §6). `--profile public`
stays the contract's spelling.

It holds no disk state, writes nothing, ships no game data, and opens no listener besides its
public socket and its control endpoint.

## Layout

- **Game-agnostic core** (`src/core`, `src/control`, `src/runtime.rs`, `src/net.rs`, `src/stun.rs`,
  `src/log.rs`): the control channel, worlds, admission state, limits and the memory budget, the
  timer wheel, events, the loop, the public socket and logs (`stun.rs`: the side channels'
  framing only). The core is deterministic and I/O-free (`core::Server`): time, randomness,
  memory and CPU are supplied by the caller, so the same API is driven by the process loop and by
  golden-vector replays.
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
cargo build --release --target x86_64-unknown-linux-musl   # the Worlds host VM
cargo test                                                   # any platform (on Windows: the tests only)
```

Set `DS_COMMIT=<40 lowercase hex>` when building to stamp the commit that `--version` and
`hello.build.commit` report (40 zeros otherwise), and `DS_PRODUCT_VERSION` the version they report
(the crate version otherwise). The client ships no `worlds.exe` any more (OD-208), so there is no
Windows VERSIONINFO (no `build.rs`): a Windows build runs the tests, and its `serve` has no control
endpoint (exit 3).

## Run

```text
worlds serve --profile public --control unix:<path> --bind <addr:port> [options]
worlds serve --version          # also: worlds --version
worlds serve --health --control unix:<path>
worlds agent [options]          # the world agent (Linux; Windows: exit 7)
worlds relay                    # not implemented yet: exit 7
```

| Option | Environment | Default |
| --- | --- | --- |
| `--profile` | `FI_DS_PROFILE` | required: `public` (its only value) |
| `--control` | `FI_DS_CONTROL` | required: `unix:<absolute path>` |
| `--game-db` | `FI_DS_GAME_DB` | none; deprecated and ignored (the catalogs come by `catalog.set` / `catalog.parts`) |
| `--grant-keys` | `FI_DS_GRANT_VERIFY_KEYS` | none (`keys.set` replaces it) |
| `--bind` | `FI_DS_BIND` | required: the one public UDP socket `addr:port` (`[::]:port` dual-stack) |
| `--max-worlds` | `FI_DS_MAX_WORLDS` | 4 (1-8) |
| `--max-seats` | `FI_DS_MAX_SEATS` | 48 (1-128) |
| `--memory-limit-mb` | `FI_DS_MEMORY_LIMIT_MB` | 400 (128-65536) |
| `--drain-seconds` | `FI_DS_DRAIN_SECONDS` | 25 (0-300) |
| `--stats-seconds` | `FI_DS_STATS_SECONDS` | 60 (0 or 10-3600) |
| `--log-level` | `FI_DS_LOG_LEVEL` | `info` (`debug` refused) |

Exit codes: 0 clean stop, 2 invalid configuration, 3 control endpoint unavailable, 4 control
authentication failed, 5 no common protocol version, 6 public socket or budget not set up,
7 role not implemented yet (`worlds relay`; `worlds agent` on Windows), 70 internal fault outside any world.

### `worlds agent` (WORLDS_OFFICIAL.md §4, §5; DEDICATED_SERVER.md §16)

The "host's launcher" of official worlds on a Worlds host: it signs every call to the server's
`/mm/v1/official` routes with the VM's node key (`FI-World-Agent`), spawns and supervises
`worlds serve --profile public --bind <FI_WORLDS_BIND>` from its own executable (restarted with
backoff; the worlds of an ended engine are `DELETE`d), keeps one spare open world per (game,
activity), mixed (`mode: "any"`) when it serves both modes (OD-210, CR-S101; the car meet lobby
keeps the first mode), announces each every 15 s with `world.state`'s peers, feeds each answer to
`world.context` and `world.admission`, and answers `catalog.need` from the server-side catalog.
SIGTERM: every world `DELETE`d, the engine shut down, exit 0.

| Option | Environment | Default |
| --- | --- | --- |
| `--server-url` | `FI_WORLDS_SERVER_URL` | required: `https://host[:port]`; `http://` only for a loopback or RFC 1918 IP literal or `localhost` |
| `--vm-id` | `FI_WORLDS_VM_ID` | required: `[a-z0-9-]{1,32}`, the `FI_WORLDS` entry |
| `--key-file` | `FI_WORLDS_KEY_FILE` | required: absolute path; base64 of the 32-byte Ed25519 seed, created once (0600) when missing |
| `--bind` | `FI_WORLDS_BIND` | required: the engine's public UDP `addr:port` (the `FI_WORLDS` endpoint's port) |
| `--control` | `FI_WORLDS_CONTROL` | `unix:/run/fi-ds/control.sock` |
| `--catalog` | `FI_WORLDS_CATALOG` | none: the server-side catalog SQLite, opened read-only and immutable |
| `--activities` | `FI_WORLDS_ACTIVITIES` | the hello's `activities` (`freeroam`, `roadtrip`); Road Trip only with catalog events |
| `--modes` | `FI_WORLDS_MODES` | `hardcore,noob` |
| `--max-worlds` | `FI_WORLDS_MAX_WORLDS` | 4 (1-8; the engine's `--max-worlds`) |
| `--max-players` | `FI_WORLDS_MAX_PLAYERS` | 12 (2-12 guests per world) |
| `--announce-seconds` | `FI_WORLDS_ANNOUNCE_SECONDS` | 15 (1-15; less only in tests) |

It logs the node key's public half once (`agent.started {publicKey}`, for the server's
`FI_WORLDS`), never the seed, a grant, an address or a body. Exit codes: 0 stopped by a signal, 2
invalid setting (`agent.config.invalid {field}`), 3 another controller holds the engine.

Logs are JSON lines on stdout, written off the loop through a bounded queue that counts what it
drops. They never carry a player identity, an address, a key or game payload bytes.

The control channel speaks JSON lines, protocol version 1: `hello`, `keys.set`, `capacity`,
`health`, `stats`, `drain`, `shutdown`, and the `world.*` commands, with events back to the one
controller session (`controller.kind: "vm"`, the agent). A Unix socket (directory `0700`, socket
`0600`) on Linux, no secret (the launcher's named pipe and its per-launch secret went with
player-hosted worlds, OD-208). A controller that leaves keeps the worlds.

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
- **Sessions:** a request from a non-controller connection other than `hello`, `health`,
  `capacity` or `stats` is `bad_request {field: "op", reason: "controller_only"}`. `unauthorized`
  closes only its connection; `proto_unsupported` ends the process only for a controller `hello`,
  never for a health probe.
- **Event `seq`** is assigned when an event leaves the bounded queue, so a drop is reported by
  `events.dropped` before the next event and the sequence the controller reads never has an
  unexplained gap; coalescing a newer `world.state` over an unsent one is not a drop.
- **Validation:** a malformed field is `bad_request` and changes nothing; a well-formed snapshot
  that fails the admission checks is `context_invalid` (or `no_keys`) and clears admission.
- **Lapse without a context:** a world that never received `world.context` counts its 45 s from
  `world.open` for the self-close of a controllerless public world.
- **`world.open` without `game`** means `"fh2"` (the only game before the field existed).
- **`world.open`** takes the tagged host (OD-186 item 7): only `{kind: "official", vmId}` (`vmId`
  `[a-z0-9-]{1,32}`; grants name it `host: {official: vmId}`, the FIUP header the official host tag,
  WORLDS_OFFICIAL.md §3). `requireProof` is read and forced; `devLanPeers: true` and `network` are refused. `hello.proto` is an integer (the step-1 range `[lo, hi]` is still read).
  `world.context` carries the context only and answers `{leaseUntilMs}`; `world.admission` carries the punches only.
- **Panics:** a panic inside a world's command, datagram or tick closes only that world
  (`world.closed {reason: "fault"}`); the panic hook only logs (anonymised); a panic on the loop
  thread is exit 70.
- **Feature `test-hooks`** adds a `debug.fault` op that panics inside a world on purpose, for the
  isolation checks; it is never enabled in a release build.

## PORT_STATUS

Step 1 was the process and its scaffold. Step 2 ported the free-roam engine (`src/fh2/engine/`)
from the Infinity C# world engine's behaviour, judged by WINDOWS' D2 golden vectors at the engine
level. Step 3 adds admission (`core/admission.rs`, `grant.rs`, `fiup.rs`) and free-roam events
(`fh2/engine/races.rs`, `options.rs`): a world serves granted players. (The private world's
network, the relay legs and the launcher's pipe went with player-hosted worlds, OD-208.)
The game features come next, each one passing its golden vectors recorded from the current C#
engine, in this recommended order:

| # | Feature | Where | Notes |
| --- | --- | --- | --- |
| 0 | NerdCore codec (`src/fh2/codec/`) | fh2 | core written for Infinity from NERDCORE_WIRE.md, the C# engine and fh2-mods as references (OD-187; no code copied): bit stream, header, reliability state, objects, synced variables, every message body but 22 (none exists); every §7 variable list (7.1-7.12), checked against the corpus's object payloads (**done**) |
| 1 | Free roam core: NerdCore handshake and refusals 1-4, acks and resends, keepalive, TimeSync, object history with `HistoryMaxEntries` (trim, then drop; 16384), queue and rate caps, quiet drop, takeover; quiet joins (T25), weather, group navigation, held opens, the car-state side channel | fh2 | **done** for free roam: the D2 Delivery, Boundary, WorldFoundation, Integration and SideChannel suites and the free-roam RelayEngineTests replay exactly |
| 2 | Admission: relay-grant verification (Ed25519), FIUP v1 proof host, held opens, kicks and revocations; proof always required (network hints and the private-address rule went with player-hosted worlds, OD-208) | core | **done** (private tests `admission.rs`) |
| 3 | Private network: STUN/NAT and UPnP wired to the world socket, punches, `world.network` | core | **removed** (OD-208: players host no world; WORLDS_OFFICIAL.md §6) |
| 4 | The one direct public socket (relay legs for private worlds: removed, OD-208) | core | **done**: official worlds share one public socket (`--bind`), a FIUP frame routed by its grant id, every other datagram by its proven endpoint; no legs, STUN, UPnP or punches (OD-189) |
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
  OpenConnection with no catalog: `ds.world.no_roadtrip` once, and a free roam world takes it or
  refusal 4 (a Road Trip or car meet world). `--game-db` is accepted and ignored.
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
- **Official worlds:** `world.open.activity` (and `carMeetLobby`) is required; `lobbies` lists only
  that activity's lobby.

## Decisions taken for private official worlds (T-L9; WORLDS_OFFICIAL.md §12.8b, T-R3)

- **How a world becomes private:** the server's `WorldView.private: true` on a `PUT`/announce
  answer. The agent then sends `world.context` with `world.private: true` (every context of that
  world from then on), stops counting the world as a spare, closes it as a group world
  (`GROUP_HOLD`, counted from the promotion) and logs `agent.world.private`. One way: a context
  without the member never makes it public again. Only a `freeroam` or `roadtrip`
  world takes it; a car meet world or a non-boolean value is
  `bad_request {field: "world.private"}`, and nothing changes.
- **The engine:** SessionHostSynced `PrivacyMode` (var 1) = 1 InviteOnly, to the members and in
  every later snapshot. The leader is the longest-present synced member (earliest sync, the
  earlier seat on a tie), picked again when it leaves; a seat that takes over the leader's own game
  (a rebind) inherits it once synced. `IsLeader` is written to every member: the leader's own
  object gets IsLeader and PlayerHost (as Road Trip always wrote), the others its relayed object's
  IsLeader, kept for later joiners' snapshots (9 more bits reserved at ingest, private only); the
  old leader is cleared before the new one is set. RequestLeaderChange (59) from the leader naming
  a synced member's player key moves it (the asker's fan-out budget pays); from anyone else, or
  for anything else, it changes nothing. DropLeadership (60) is not read. A member's own
  IsLeader and PlayerHost writes are stripped before they are kept or relayed (private only;
  PlayerHost while the player tail's widths are known), so nobody can crown itself.
- **The agent** only promotes a free roam or Road Trip spare; `private` on any other world's
  answer is ignored.
- **Private Road Trip (A7):** no OD-194 majority and no send-back. The vote is shown as usual; the
  leader's choice decides and closes it 10 s later once two players are in; without that choice
  (or alone) the vote is shown again for 60 s. A member alone after the vote is never sent back;
  events still need two (OD-183).
- **`world.state`** adds `private: true` and, while somebody leads, `leader` (XUID); the agent
  announces `leader` for a private world only, at once when it changes. A public world's state,
  announce and bytes are unchanged.

## Acknowledgements

The fh2-mods project's online research (https://github.com/conner-mcvay/fh2-mods, `tools/online`, by conner-mcvay and
contributors) was an inspiration and a reference for this work. No code from it is used here.

**References (owner decision OD-187, final rule).** This server was written for Infinity with these references: the
Infinity C# world engine (the launcher's former in-process WorldServer, client `b43618a`) for every behaviour, and its
golden vectors; our NERDCORE_WIRE.md for the game's network format; fh2-mods' code as inspiration and reference; RFC
8489/5389 (STUN) for the side channels' framing (the UPnP and NAT discovery modules built from the UPnP IGD and SSDP
specifications went with player-hosted worlds, OD-208). No code is
copied from fh2-mods or from the retired Beta relay (`deprecated/native-relay`); the STUN and car-state
modules were rewritten from those specifications for this server, and no new reverse engineering was done for it.
