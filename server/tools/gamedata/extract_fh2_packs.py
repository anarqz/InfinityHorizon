#!/usr/bin/env python3
"""Extract the FH2 car-pack cars the server accepts and names (OD-231, ZephyrSkies FM5/FM6 packs).

    python3 -I tools/gamedata/extract_fh2_packs.py <packs dir> gamedata/fh2/catalog.json \
        > gamedata/fh2/packs.json

`<packs dir>` holds one folder per pack with its `mod.json`, `database.json` and (optional)
`strings.json`, as the Infinity mod runtime installs them. Reads each pack's
`tables.Data_Car.insert` rows and prints the compact JSON that `fi_gamedb::Catalog::fh2` merges
over the base catalog: per car its make, model, year, media name, stock class, display PI, stock
top speed (m/s, from `TopSpeed-mph` as `extract_fh2.py` derives it), base cost, and the pack
(id, version, source game FM5/FM6). Nothing else of the packs is copied.

Class and PI: FH2's classes are D (0) to X (6) with display PI 1 to 999. A pack car whose class is
FM's R (7), any other class outside 0..6, or whose PI is outside 1..999 is stored as X, PI 999: the
game recalculates such a car to X 999 when it is bought (ZephyrSkies, 2026-11-09).

Names: a `_&<u32>` reference's low 16 bits are the key of the pack's `strings.json`
`languages.EN.<table>`. The model is `Data_Car.DisplayName`. The make is the `List_CarMake` row a
pack inserts (any pack: the common packs add makes the others use), its `DisplayName` string, else
the base catalog's make of that `MakeID`. An unresolved name, a car id the base catalog already
lists, a car id two packs insert, or a make two packs name differently is an error. The JSON is
deterministic (sorted keys, cars by id).
"""

import json
import os
import re
import sys


def str_key(reference):
    m = re.fullmatch(r"_&(\d+)", reference or "")
    return int(m.group(1)) & 0xFFFF if m else None


def load(path):
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def game_of(pack_id):
    if "fm6" in pack_id:
        return "FM6"
    if "fm5" in pack_id:
        return "FM5"
    raise SystemExit("pack %s: no source game in its id" % pack_id)


X_CLASS, X_PI = 6, 999


def fh2_class(class_id, pi):
    """The (class, display PI) FH2 shows for a pack car: X 999 outside FH2's classes and PIs."""
    if 0 <= class_id <= X_CLASS and 1 <= pi <= X_PI:
        return class_id, pi
    return X_CLASS, X_PI


def extract(packs_dir, base):
    stock_makes = {c["makeId"]: c["make"] for c in base["cars"]}
    stock_ids = {c["id"] for c in base["cars"]}
    packs = []
    for name in sorted(os.listdir(packs_dir)):
        root = os.path.join(packs_dir, name)
        if not os.path.isfile(os.path.join(root, "mod.json")):
            continue
        mod = load(os.path.join(root, "mod.json"))
        db_path = os.path.join(root, mod.get("database") or "database.json")
        db = load(db_path) if os.path.isfile(db_path) else {"tables": {}}
        strings_path = os.path.join(root, "strings.json")
        strings = load(strings_path)["languages"].get("EN", {}) if os.path.isfile(strings_path) else {}
        packs.append((mod, db["tables"], strings))

    def text(strings, table, reference, what):
        key = str_key(reference)
        value = strings.get(table, {}).get(str(key)) if key is not None else None
        if not value:
            raise SystemExit("%s: unresolved %s %r" % (what[0], what[1], reference))
        return value

    makes = {}
    for mod, tables, strings in packs:
        for row in tables.get("List_CarMake", {}).get("insert", []):
            make = text(strings, "List_CarMake", row.get("DisplayName"), (mod["id"], "make"))
            if makes.setdefault(row["ID"], make) != make:
                raise SystemExit("make %s named %r and %r" % (row["ID"], makes[row["ID"]], make))

    cars = {}
    for mod, tables, strings in packs:
        for row in tables.get("Data_Car", {}).get("insert", []):
            cid = row["Id"]
            if cid in stock_ids or cid in cars:
                raise SystemExit("pack %s: car %s already listed" % (mod["id"], cid))
            make_id = row["MakeID"]
            make = makes.get(make_id, stock_makes.get(make_id))
            if make is None:
                raise SystemExit("pack %s: car %s has unknown make %s" % (mod["id"], cid, make_id))
            class_id, pi = fh2_class(row["ClassID"], int(round(row["PerformanceIndex"] * 1000)))
            cars[cid] = {
                "id": cid,
                "year": row["Year"],
                "makeId": make_id,
                "make": make,
                "model": text(strings, "Data_Car", row.get("DisplayName"), (mod["id"], "model")),
                "mediaName": row["MediaName"],
                "classId": class_id,
                "pi": pi,
                "topSpeedMps": round((row.get("TopSpeed-mph") or 0.0) * 0.44704, 3),
                "baseCost": int(row["BaseCost"]),
                "pack": {"id": mod["id"], "version": mod["version"], "game": game_of(mod["id"])},
            }
    return {"cars": [cars[k] for k in sorted(cars)]}


def main(argv):
    if len(argv) != 3:
        sys.exit(__doc__)
    out = extract(argv[1], load(argv[2]))
    json.dump(out, sys.stdout, ensure_ascii=False, sort_keys=True, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(sys.argv)
