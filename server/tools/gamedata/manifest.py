#!/usr/bin/env python3
"""Write or check the manifest of a game data directory (FI_GAME_DATA_DIR).

    python3 -I tools/gamedata/manifest.py write <dir>   # lists every file under <dir> with its SHA-256
    python3 -I tools/gamedata/manifest.py check <dir>   # exit 1 when a file is missing, extra or changed

The server reads `<dir>/manifest.json` at start and refuses a listed file whose SHA-256 differs, so
run `write` after rebuilding or editing any file. The output is deterministic (sorted names,
lowercase hex, one trailing newline).
"""

import hashlib
import json
import os
import sys

FORMAT = "fi-gamedata/1"
MANIFEST = "manifest.json"


def scan(root):
    files = {}
    for base, dirs, names in os.walk(root):
        dirs[:] = sorted(d for d in dirs if not d.startswith("."))
        for name in sorted(names):
            if name.startswith("."):
                continue
            path = os.path.join(base, name)
            rel = os.path.relpath(path, root).replace(os.sep, "/")
            if rel == MANIFEST:
                continue
            with open(path, "rb") as f:
                files[rel] = hashlib.sha256(f.read()).hexdigest()
    return files


def render(files):
    return json.dumps({"format": FORMAT, "files": files}, indent=1, sort_keys=True) + "\n"


def main(argv):
    if len(argv) != 3 or argv[1] not in ("write", "check"):
        print(__doc__.strip(), file=sys.stderr)
        return 2
    root = argv[2]
    text = render(scan(root))
    path = os.path.join(root, MANIFEST)
    if argv[1] == "write":
        with open(path, "w", encoding="utf-8", newline="\n") as f:
            f.write(text)
        return 0
    try:
        with open(path, encoding="utf-8") as f:
            current = f.read()
    except OSError as e:
        print(f"{path}: {e}", file=sys.stderr)
        return 1
    if current != text:
        print(f"{path}: out of date (run: manifest.py write {root})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
