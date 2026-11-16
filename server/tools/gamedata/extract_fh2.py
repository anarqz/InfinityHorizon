#!/usr/bin/env python3
"""Extract the server's FH2 catalog from the game's own database (OD-32, OD-48).

    python3 -I tools/gamedata/extract_fh2.py <gamedbRC.slt> <stringtables dir or EN.zip> \
        [<pt-BR stringtable zip>] > gamedata/fh2/catalog.json
    python3 -I tools/gamedata/extract_fh2.py --parts <gamedbRC.slt> > gamedata/fh2/parts.json

Reads `media/stripped/gamedbRC.slt` (plain SQLite, opened read-only) and the `Tracks.str` table
of the English and Brazilian Portuguese stringtable zips, and prints the compact JSON that
`fi_gamedb::Catalog::fh2` reads (game data `fh2/catalog.json`): cars (make, class, display PI, stock top speed, base cost),
classes, car buckets (hero cars first), tracks (length, reverse, names, cross-country, and the lap
count of the circuits the game itself races over more than one lap), perk costs and the starter
pick's cars (CR-S24). Only what the server's gates,
drivatars, rivals and texts need; nothing else of the game is copied. The JSON is deterministic
(sorted keys and rows), so a re-extraction of the same files is byte-identical.

`--parts` prints the parts shop's list prices instead (CR-S24): `partPrices` maps each
`Career_PurchasedParts.PartEnum` (as a string) to `[[PartId, Price], ...]`, every row of its table
by id, price 0 included, so a missing id means the part does not exist.
"""

import hashlib
import json
import re
import sqlite3
import struct
import sys
import zipfile


def split_words(text):
    """'AstonMartin' -> 'Aston Martin', 'SupraRZ' -> 'Supra RZ', 'Dino246' -> 'Dino 246'."""
    out = []
    for i, c in enumerate(text):
        if i > 0 and c != " " and text[i - 1] != " ":
            prev = text[i - 1]
            nxt = text[i + 1] if i + 1 < len(text) else ""
            if (
                (c.isupper() and prev.islower())
                or (c.isupper() and prev.isupper() and nxt.islower())
                or (c.isdigit() and prev.isalpha())
                or (c.isalpha() and prev.isdigit() and c.isupper())
            ):
                out.append(" ")
        out.append(c)
    return "".join(out)


def model_from_media(media):
    """'TOY_SupraRZ_98' -> 'Supra RZ'."""
    parts = media.split("_")
    if len(parts) > 1:
        parts = parts[1:]
    if len(parts) > 1 and len(parts[-1]) <= 2 and parts[-1].isdigit():
        parts = parts[:-1]
    return split_words(" ".join(parts))


def lsb2(table):
    """The strings of an LSB2 .str table by key (the Beta's strtable.rs)."""
    out = {}
    if not table.startswith(b"LSB2") or len(table) < 0x2C:
        return out
    (count,) = struct.unpack_from("<I", table, 0x28)
    texts = 0x2C + 6 * count + 6
    for i in range(count):
        e = 0x2C + 6 * i
        if e + 6 > len(table):
            break
        key, offset = struct.unpack_from("<HI", table, e)
        at = texts + 2 * offset
        units = []
        while at + 2 <= len(table):
            (u,) = struct.unpack_from("<H", table, at)
            if u == 0:
                break
            units.append(u)
            at += 2
        out[key] = struct.pack("<%dH" % len(units), *units).decode("utf-16-le", "replace")
    return out


def tracks_table(path):
    if not path:
        return {}
    if not path.endswith(".zip"):
        path = path.rstrip("/") + "/EN.zip"
    with zipfile.ZipFile(path) as z:
        return lsb2(z.read("Tracks.str"))


# PartEnum -> gamedb table, as the parts shop resolves it (F_1402cebc0: 0-42 from the game's name
# table at 0x142e797f0, 43 special-cased to List_Wheels; hardcore-save-fields.md §3.3).
PART_TABLES = [
    "List_UpgradeEngine",
    "List_UpgradeDrivetrain",
    "List_UpgradeCarBody",
    "List_UpgradeMotor",
    "List_UpgradeBrakes",
    "List_UpgradeSpringDamper",
    "List_UpgradeAntiSwayFront",
    "List_UpgradeAntiSwayRear",
    "List_UpgradeTireCompound",
    "List_UpgradeRearWing",
    "List_UpgradeRimSizeFront",
    "List_UpgradeRimSizeRear",
    "List_UpgradeEngineCamshaft",
    "List_UpgradeEngineValves",
    "List_UpgradeEngineDisplacement",
    "List_UpgradeEnginePistonsCompression",
    "List_UpgradeEngineFuelSystem",
    "List_UpgradeEngineIgnition",
    "List_UpgradeEngineExhaust",
    "List_UpgradeEngineIntake",
    "List_UpgradeEngineFlywheel",
    "List_UpgradeEngineManifold",
    "List_UpgradeEngineRestrictorPlate",
    "List_UpgradeEngineOilCooling",
    "List_UpgradeEngineTurboSingle",
    "List_UpgradeEngineTurboTwin",
    "List_UpgradeEngineTurboQuad",
    "List_UpgradeEngineCSC",
    "List_UpgradeEngineDSC",
    "List_UpgradeEngineIntercooler",
    "List_UpgradeDrivetrainClutch",
    "List_UpgradeDrivetrainTransmission",
    "List_UpgradeDrivetrainDriveline",
    "List_UpgradeDrivetrainDifferential",
    "List_UpgradeCarBodyFrontBumper",
    "List_UpgradeCarBodyRearBumper",
    "List_UpgradeCarBodyHood",
    "List_UpgradeCarBodySideSkirt",
    "List_UpgradeCarBodyTireWidthFront",
    "List_UpgradeCarBodyTireWidthRear",
    "List_UpgradeCarBodyWeight",
    "List_UpgradeCarBodyChassisStiffness",
    "List_UpgradeMotorParts",
    "List_Wheels",
]


def parts(db, source):
    """The parts JSON: one compact line per PartEnum, sorted keys at the top."""
    prices = {}
    for enum, table in enumerate(PART_TABLES):
        rows = db.execute('SELECT Id, Price FROM "%s" ORDER BY Id' % table).fetchall()
        prices[str(enum)] = [[pid, int(price)] for pid, price in rows]
    lines = ["{", ' "partPrices": {']
    keys = sorted(prices)
    for i, key in enumerate(keys):
        tail = "," if i + 1 < len(keys) else ""
        lines.append("  %s: %s%s" % (json.dumps(key), json.dumps(prices[key], separators=(",", ":")), tail))
    lines.append(" },")
    lines.append(' "source": ' + json.dumps(source, sort_keys=True, separators=(", ", ": ")))
    lines.append("}")
    return "\n".join(lines) + "\n"


def str_key(reference):
    m = re.fullmatch(r"_&(\d+)", reference or "")
    return int(m.group(1)) & 0xFFFF if m else None


def main(argv):
    parts_only = len(argv) > 1 and argv[1] == "--parts"
    if parts_only:
        argv = argv[:1] + argv[2:]
    if len(argv) < 2:
        sys.exit(__doc__)
    slt = argv[1]
    names = {"en": tracks_table(argv[2] if len(argv) > 2 else None)}
    if len(argv) > 3:
        names["pt-BR"] = tracks_table(argv[3])
    with open(slt, "rb") as f:
        digest = hashlib.sha256(f.read()).hexdigest()
    db = sqlite3.connect("file:%s?mode=ro" % slt, uri=True)
    if parts_only:
        sys.stdout.write(parts(db, {"file": "gamedbRC.slt", "sha256": digest}))
        return

    makes = {mid: split_words(base) for mid, base in db.execute("SELECT ID, IconPathBase FROM List_CarMake")}
    cars = []
    for cid, year, make_id, media, class_id, pi, top_mph, base_cost in db.execute(
        'SELECT Id, Year, MakeID, MediaName, ClassID, PerformanceIndex, "TopSpeed-mph", BaseCost'
        " FROM Data_Car ORDER BY Id"
    ):
        cars.append(
            {
                "id": cid,
                "year": year,
                "makeId": make_id,
                "make": makes.get(make_id, ""),
                "model": model_from_media(media),
                "mediaName": media,
                "classId": class_id,
                "pi": int(round(pi * 1000)),
                "topSpeedMps": round((top_mph or 0.0) * 0.44704, 3),
                "baseCost": int(base_cost),
            }
        )
    classes = [
        {"id": cid, "name": prefix.replace("CLASS_", "", 1), "maxPi": max_pi}
        for cid, prefix, max_pi in db.execute(
            "SELECT Id, BadgeTexturePathPrefix, MaxDisplayPerformanceIndex FROM CarClasses ORDER BY Id"
        )
    ]
    buckets = [
        {"carId": car, "bucket": bucket, "hero": bool(hero)}
        for car, bucket, hero in db.execute(
            "SELECT CarId, CarBucket, BucketHero FROM Data_Car_Buckets ORDER BY CarBucket, BucketHero DESC, CarId"
        )
    ]
    # Circuits: the tracks the game's own online events or career races run over more than one lap
    # (parity with other projects, S2). Their lap count is the online events' most common one, else the career's.
    circuit_laps = {}
    for table in ("AnthemEventRaces", "AnthemOnlineEvents"):
        for tid, laps, n in db.execute(
            "SELECT TrackId, NumLaps, COUNT(*) FROM %s WHERE NumLaps > 1 GROUP BY TrackId, NumLaps"
            " ORDER BY TrackId, COUNT(*), NumLaps" % table
        ):
            circuit_laps[tid] = laps
    tracks = []
    for tid, media, length, reverse, display, cross in db.execute(
        "SELECT id, MediaName, Length, IsReverse, DisplayName, UseCrossCountryAI FROM Tracks ORDER BY id"
    ):
        key = str_key(display)
        track = {"id": tid, "mediaName": media, "lengthM": length, "reverse": bool(reverse), "names": {}}
        if tid in circuit_laps:
            track["circuitLaps"] = circuit_laps[tid]
        if cross:
            track["crossCountry"] = True
        for lang, table in names.items():
            if key is not None and table.get(key):
                track["names"][lang] = table[key]
        tracks.append(track)
    perks = [{"id": pid, "cost": int(cost)} for pid, cost in db.execute("SELECT Id, Cost FROM Perks ORDER BY Id")]
    # The opening's in-world car select (States::SelectCarInWorld) offers these and grants the pick
    # free with no origin bit in Career_Garage.Flags (hardcore-save-fields.md §3.2).
    starters = [cid for (cid,) in db.execute("SELECT DISTINCT CarId FROM IEAutoShowCars ORDER BY CarId")]
    out = {
        "source": {"file": "gamedbRC.slt", "sha256": digest},
        "cars": cars,
        "classes": classes,
        "buckets": buckets,
        "tracks": tracks,
        "perks": perks,
        "starterCars": starters,
    }
    json.dump(out, sys.stdout, ensure_ascii=False, sort_keys=True, indent=1)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main(sys.argv)
