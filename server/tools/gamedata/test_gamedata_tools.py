"""Synthetic tests of build.py, manifest.py and extract_schema.py's registration scan:
python3 -I tools/gamedata/test_gamedata_tools.py"""

import hashlib
import json
import os
import struct
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import build  # noqa: E402
import extract_schema  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def manifest(*args):
    return subprocess.run([sys.executable, "-I", os.path.join(HERE, "manifest.py"), *args],
                          capture_output=True, text=True)


class ManifestTest(unittest.TestCase):
    def test_write_lists_every_file_and_check_sees_a_change(self):
        with tempfile.TemporaryDirectory() as d:
            os.makedirs(os.path.join(d, "fh2"))
            with open(os.path.join(d, "fh2", "a.json"), "wb") as f:
                f.write(b"{}")
            self.assertEqual(manifest("write", d).returncode, 0)
            with open(os.path.join(d, "manifest.json")) as f:
                m = json.load(f)
            self.assertEqual(m, {"format": "fi-gamedata/1",
                                 "files": {"fh2/a.json": hashlib.sha256(b"{}").hexdigest()}})
            self.assertEqual(manifest("check", d).returncode, 0)
            with open(os.path.join(d, "fh2", "a.json"), "wb") as f:
                f.write(b"{ }")
            self.assertEqual(manifest("check", d).returncode, 1)


class BuildTest(unittest.TestCase):
    def test_the_plan_follows_the_options(self):
        plan = build.steps({"out": "/o", "fh2": "/g", "packs": "/p", "fm5_exe": "/f.exe"})
        self.assertEqual([p[0] for p in plan], build.BUILT[:4] + ["fm5/xtsw-schema.json"])
        catalog = [p for p in plan if p[0] == "fh2/catalog.json"][0]
        self.assertEqual(catalog[2][0], os.path.join("/g", "media", "stripped", "gamedbRC.slt"))
        self.assertEqual(build.steps({"out": "/o"}), [])

    def test_every_file_the_server_reads_is_listed_once(self):
        files = build.BUILT + build.MANUAL
        self.assertEqual(len(files), len(set(files)))
        self.assertEqual(len(files), 19)

    def test_missing_files_stop_the_build(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(build.main(["build.py", "--out", d]), 1)
            # Without --packs an empty pack list is written; the rest stays missing.
            self.assertTrue(os.path.isfile(os.path.join(d, "fh2", "packs.json")))
            self.assertNotIn("fh2/packs.json", build.missing(d))
            self.assertIn("fh2/catalog.json", build.missing(d))
            self.assertFalse(os.path.exists(os.path.join(d, "manifest.json")))

    def test_the_published_tables_are_copied(self):
        with tempfile.TemporaryDirectory() as tables, tempfile.TemporaryDirectory() as out:
            for name in build.MANUAL:
                os.makedirs(os.path.join(tables, os.path.dirname(name)), exist_ok=True)
                with open(os.path.join(tables, name), "w") as f:
                    f.write(name)
            self.assertEqual(build.copy_tables(out, tables), build.MANUAL)
            with open(os.path.join(out, "fh2", "economy.json")) as f:
                self.assertEqual(f.read(), "fh2/economy.json")
            self.assertEqual(sorted(build.missing(out)), sorted(build.BUILT))
            # Without FM5 its schema is not required.
            self.assertEqual(sorted(build.missing(out, fm5=False)),
                             sorted(f for f in build.BUILT if f not in build.FM5))

    def test_a_server_without_fm5_gets_a_complete_directory(self):
        with tempfile.TemporaryDirectory() as out:
            for name in build.BUILT:
                if name in build.FM5:
                    continue
                os.makedirs(os.path.join(out, os.path.dirname(name)), exist_ok=True)
                with open(os.path.join(out, name), "w") as f:
                    f.write("{}")
            # The published tables come from this tree's gamedata/ (the copy step).
            if os.path.isdir(build.TABLES) and all(
                    os.path.isfile(os.path.join(build.TABLES, m)) for m in build.MANUAL):
                self.assertEqual(build.main(["build.py", "--out", out]), 0)
                with open(os.path.join(out, "manifest.json")) as f:
                    listed = json.load(f)["files"]
                self.assertNotIn("fm5/xtsw-schema.json", listed)
                self.assertIn("fh2/catalog.json", listed)


class FakeImage:
    """An image whose .text holds made-up initializers."""

    def __init__(self, text):
        self.img = b"\0" * 0x1000 + text
        self.secs = {".text": (0x1000, len(text))}


def initializer(at, desc, cell, n, target):
    """The four instructions at image offset `at` (all displacements relative to BASE)."""
    base = extract_schema.BASE
    code = b"\x48\x8d\x15" + struct.pack("<i", desc - (base + at + 7))
    code += b"\x48\x8d\x0d" + struct.pack("<i", cell - (base + at + 14))
    code += b"\x41\xb8" + struct.pack("<I", n)
    code += b"\xe9" + struct.pack("<i", target - (base + at + 25))
    return code


class SchemaScanTest(unittest.TestCase):
    def test_the_registrar_is_the_shared_jump_target(self):
        base = extract_schema.BASE
        reg, other = base + 0x9000, base + 0x9900
        text = b"\x90"
        rows = [(0x5000, 0x6000, 3, reg), (0x5100, 0x6008, 1, reg), (0x5200, 0x6010, 2, other)]
        for desc, cell, n, target in rows:
            text += initializer(0x1000 + len(text), base + desc, base + cell, n, target) + b"\x90"
        regs, found = extract_schema.find_registrations(FakeImage(text))
        self.assertEqual(found, reg)
        self.assertEqual(regs, {base + 0x6000: (base + 0x5000, 3), base + 0x6008: (base + 0x5100, 1)})
        regs, found = extract_schema.find_registrations(FakeImage(text), other)
        self.assertEqual(regs, {base + 0x6010: (base + 0x5200, 2)})

    def test_an_image_without_initializers_is_refused(self):
        with self.assertRaises(SystemExit):
            extract_schema.find_registrations(FakeImage(b"\x90" * 64))


if __name__ == "__main__":
    unittest.main()
