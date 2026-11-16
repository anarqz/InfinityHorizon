#!/usr/bin/env python3
"""Rebuild a game data directory (FI_GAME_DATA_DIR) from the operator's own game install.

    python3 -I tools/gamedata/build.py --out <dir> --fh2 <Forza Horizon 2 install> \
        [--pt-br <pt-BR stringtable zip>] [--packs <car packs dir>] [--fm5-exe <FM5 executable>]

Runs the extractors of this folder and writes `<dir>/manifest.json`:

| File | Built by | From |
|---|---|---|
| fh2/xtsw-schema.json | extract_schema.py | <fh2>/forza_durango_release_final.exe |
| fh2/catalog.json | extract_fh2.py | <fh2>/media/stripped/gamedbRC.slt, stringtables/EN.zip (+ --pt-br) |
| fh2/parts.json | extract_fh2.py --parts | <fh2>/media/stripped/gamedbRC.slt |
| fh2/packs.json | extract_fh2_packs.py | --packs (the installed car packs), else an empty list |
| fm5/xtsw-schema.json | extract_schema.py | --fm5-exe (only for a server that serves FM5) |

The other files the server reads (`MANUAL` below) are not files of an install: they are our own
tables (identifiers, limits, layouts and corrections we wrote or measured), published with the
server in the `gamedata/` folder next to `tools/` (TABLES). The tool copies each of them into
`<dir>`, then stops with the list of anything still missing, or writes the manifest. The server
refuses to start without any of them (tools/gamedata/README.md).

Without `--fm5-exe` the FM5 schema is not built and the manifest lists only what was: such a
directory serves a server whose `FI_GAMES` does not include `fm5` (the server requires a game's
files only when it serves that game).

The FH2 catalog matches the official one byte for byte only when built with the install's pt-BR
stringtable (`--pt-br <fh2>/media/stripped/stringtables/br.zip`). Without `--packs`,
`fh2/packs.json` is `{"cars": []}`, a valid result for a server without car packs.
"""

import json
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
# The published tables (MANUAL): `gamedata/` at the root of the tree holding `tools/`.
TABLES = os.path.join(os.path.dirname(os.path.dirname(HERE)), "gamedata")

# Every file the server reads (crate fi-surfaces, `gamedata::load`).
BUILT = ["fh2/xtsw-schema.json", "fh2/catalog.json", "fh2/parts.json", "fh2/packs.json", "fm5/xtsw-schema.json"]
# The BUILT files only a server serving that game reads (FI_GAMES), built only from its input.
FM5 = ["fm5/xtsw-schema.json"]
MANUAL = [
    "fh2/car-names.json",
    "fh2/card-ids.json",
    "fh2/economy-settings.json",
    "fh2/economy.json",
    "fh2/gate-confirmations.json",
    "fh2/gate-tables.json",
    "fh2/leaderboards.json",
    "fh2/playerdb-schema.sql",
    "fh2/stats-boards-v2.json",
    "fh2/stats-extras.json",
    "fh2/stats-layout.json",
    "fh2/storefront-keywords.json",
    "fh2/topspeed.tsv",
    "guard/limits.json",
]


def run(script, args, out_path):
    """Runs `script` (of this folder) with `args`, its stdout into `out_path`."""
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    with open(out_path + ".tmp", "wb") as out:
        subprocess.run([sys.executable, "-I", os.path.join(HERE, script), *args], stdout=out, check=True)
    os.replace(out_path + ".tmp", out_path)


def steps(opts):
    """(file, script, args, writes-stdout) for every file the options can build."""
    out = opts["out"]
    plan = []
    fh2 = opts.get("fh2")
    if fh2:
        stripped = os.path.join(fh2, "media", "stripped")
        slt = os.path.join(stripped, "gamedbRC.slt")
        strings = [os.path.join(stripped, "stringtables", "EN.zip")]
        if opts.get("pt_br"):
            strings.append(opts["pt_br"])
        plan.append(("fh2/xtsw-schema.json", "extract_schema.py",
                     [os.path.join(fh2, "forza_durango_release_final.exe"), os.path.join(out, "fh2/xtsw-schema.json")], False))
        plan.append(("fh2/catalog.json", "extract_fh2.py", [slt, *strings], True))
        plan.append(("fh2/parts.json", "extract_fh2.py", ["--parts", slt], True))
        if opts.get("packs"):
            plan.append(("fh2/packs.json", "extract_fh2_packs.py",
                         [opts["packs"], os.path.join(out, "fh2/catalog.json")], True))
    if opts.get("fm5_exe"):
        plan.append(("fm5/xtsw-schema.json", "extract_schema.py",
                     [opts["fm5_exe"], os.path.join(out, "fm5/xtsw-schema.json")], False))
    return plan


def copy_tables(out, tables=None):
    """Copies every MANUAL file from `tables` (default TABLES) into `out`; returns those copied."""
    tables = TABLES if tables is None else tables
    copied = []
    for name in MANUAL:
        src = os.path.join(tables, name)
        if os.path.isfile(src):
            dst = os.path.join(out, name)
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            if os.path.abspath(src) != os.path.abspath(dst):
                shutil.copyfile(src, dst)
            copied.append(name)
    return copied


def missing(out, fm5=True):
    """The files the server needs that `out` does not hold (FM5's only with `fm5`)."""
    needed = [f for f in BUILT if fm5 or f not in FM5] + MANUAL
    return [f for f in needed if not os.path.isfile(os.path.join(out, f))]


def parse(argv):
    opts, flags = {}, {"--out": "out", "--fh2": "fh2", "--pt-br": "pt_br", "--packs": "packs", "--fm5-exe": "fm5_exe"}
    i = 1
    while i < len(argv):
        if argv[i] not in flags or i + 1 >= len(argv):
            raise SystemExit(__doc__)
        opts[flags[argv[i]]] = argv[i + 1]
        i += 2
    if "out" not in opts:
        raise SystemExit(__doc__)
    return opts


def main(argv):
    opts = parse(argv)
    out = opts["out"]
    for name, script, args, to_stdout in steps(opts):
        print(f"building {name}", file=sys.stderr)
        if to_stdout:
            run(script, args, os.path.join(out, name))
        else:
            os.makedirs(os.path.join(out, os.path.dirname(name)), exist_ok=True)
            subprocess.run([sys.executable, "-I", os.path.join(HERE, script), *args], check=True)
    packs = os.path.join(out, "fh2/packs.json")
    if not opts.get("packs") and not os.path.exists(packs):
        os.makedirs(os.path.dirname(packs), exist_ok=True)
        with open(packs, "w", encoding="utf-8") as f:
            f.write(json.dumps({"cars": []}, indent=1) + "\n")
    copied = copy_tables(out)
    print(f"copied {len(copied)} published tables from {TABLES}", file=sys.stderr)
    fm5 = bool(opts.get("fm5_exe"))
    if not fm5:
        print("no --fm5-exe: FM5 not built; serve FI_GAMES without fm5", file=sys.stderr)
    lacking = missing(out, fm5)
    if lacking:
        print("still missing (see tools/gamedata/README.md):", file=sys.stderr)
        for name in lacking:
            print(f"  {name}", file=sys.stderr)
        return 1
    subprocess.run([sys.executable, "-I", os.path.join(HERE, "manifest.py"), "write", out], check=True)
    print(f"{out}: complete, manifest written", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
