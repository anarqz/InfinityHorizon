# tools/gamedata: the server's game data, rebuilt from your own install

The server compiles and ships with no file of the game. At start it reads the tables it needs from
the directory the setting `FI_GAME_DATA_DIR` names, checks every file against that directory's
`manifest.json` and refuses to start, naming the file, when one is missing, changed or malformed.
You build the directory from your own copy of the game with the tools here; nothing of the game is
distributed with the server.

## Layout

```text
<FI_GAME_DATA_DIR>/
  manifest.json                 {"format": "fi-gamedata/1", "files": {"<name>": "<sha256>", ...}}
  fh2/xtsw-schema.json          Forza Horizon 2's web-service schema (types, methods, capacities)
  fh2/catalog.json              cars, classes, race buckets, tracks, perk costs
  fh2/parts.json                the parts shop's list prices
  fh2/packs.json                cars of installed car packs ({"cars": []} without packs)
  fh2/car-names.json            car make and model display corrections, by car id
  fh2/card-ids.json             player card badge and title ids
  fh2/economy.json              level curve, skill tokens, tombola and per-race payout limits
  fh2/economy-settings.json     User.GetEconomySettings' values
  fh2/gate-confirmations.json   PI class bands
  fh2/gate-tables.json          traffic cars no player may drive; enforced stats-board maxima
  fh2/leaderboards.json         speed cameras, tracks and Showcase boards
  fh2/playerdb-schema.sql       the save's PlayerDatabase DDL of the tables the server reads
  fh2/stats-boards-v2.json      stats boards' score types and maxima
  fh2/stats-extras.json         Statistics screen units and extras
  fh2/stats-layout.json         Statistics screen layout
  fh2/storefront-keywords.json  Storefront share keywords, in id order (never reorder)
  fh2/topspeed.tsv              per-car upgraded top-speed bounds
  guard/limits.json             the Hardcore guard's tuned limits (a server's own calibration)
  fm5/xtsw-schema.json          Forza Motorsport 5's web-service schema
```

## Build

Python 3.11+; `pefile` for the schema extraction (`pip install pefile`).

```sh
python3 -I tools/gamedata/build.py --out /srv/gamedata --fh2 "<FH2 install>" \
    [--pt-br "<pt-BR stringtable zip>"] [--packs "<installed car packs>"] [--fm5-exe "<FM5 exe>"]
```

`build.py` runs, in order:

| File | Tool | Input |
|---|---|---|
| fh2/xtsw-schema.json | `extract_schema.py` | `<FH2>/forza_durango_release_final.exe` (unpacked PC executable) |
| fh2/catalog.json | `extract_fh2.py` | `<FH2>/media/stripped/gamedbRC.slt`, `stringtables/EN.zip` (+ `--pt-br`) |
| fh2/parts.json | `extract_fh2.py --parts` | `gamedbRC.slt` |
| fh2/packs.json | `extract_fh2_packs.py` | each pack's `mod.json`, `database.json`, `strings.json` |
| fm5/xtsw-schema.json | `extract_schema.py` | Forza Motorsport 5's executable (only for `FI_GAMES` with `fm5`; without `--fm5-exe` it is not built) |

and then `manifest.py write <out>`. After editing any file by hand, run
`python3 -I tools/gamedata/manifest.py write <dir>` (`check` verifies it).

The FH2 catalog matches the official one byte for byte only with the install's pt-BR stringtable
(`--pt-br "<FH2>/media/stripped/stringtables/br.zip"`). Without `--packs`, `fh2/packs.json` is
`{"cars": []}`: a valid result for a server without car packs.

The other files are not files of an install: they are our own tables, read from the game's code
or traced while it ran (economy numbers of the game's code and tunables, PI bands, stats boards,
card ids, leaderboards, top speeds, the Statistics screen, the save's DDL, the Storefront
keywords, the economy settings, car display names and the gate tables), plus the Hardcore
guard's limits (`guard/limits.json`: the published copy holds neutral, permissive values; set
your own). They are published with
the server in the `gamedata/` folder next to `tools/`; `build.py` copies them into `<out>`, then
writes the manifest, or stops with the list of anything still missing.

Then run the server with `FI_GAME_DATA_DIR=<out>` (mount it read-only in a container).

## Tests

Synthetic input only:

```sh
python3 -I tools/gamedata/test_gamedata_tools.py
python3 -I tools/gamedata/test_extract_fh2_packs.py
```
