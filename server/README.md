# Infinity Horizon server

The server side of Infinity Horizon: the account, game and save APIs (`services/platform`), the
migrations and scheduled jobs (`services/cli`), the relay (`services/relay`) and the server
directory (`services/beholder`), with their libraries in `crates/`. Licensed under AGPL-3.0
(`LICENSE`, `NOTICE`).

The world engine (lobbies and free-roam worlds) is `../native/worlds`.

## Build

```sh
cargo build --locked --workspace --release
```

The toolchain is pinned in `rust-toolchain.toml`.

## Game data

The server ships no game file. Before it starts it needs a game data directory
(`FI_GAME_DATA_DIR`), built from your own Forza Horizon 2 install:

```sh
python3 -I tools/gamedata/build.py --out <dir> --fh2 <Forza Horizon 2 install> \
    --pt-br <Forza Horizon 2 install>/media/stripped/stringtables/br.zip
```

See `tools/gamedata/README.md`.

## Running your own stack

The self-hosting guide (compose file, settings, keys, a world node and pointing the launcher at
your server) is being written; until it lands, this folder only builds.
