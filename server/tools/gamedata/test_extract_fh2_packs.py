"""Synthetic tests of extract_fh2_packs.py: python3 -I tools/gamedata/test_extract_fh2_packs.py"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from extract_fh2_packs import extract, fh2_class  # noqa: E402

BASE = {"cars": [{"id": 1, "makeId": 13, "make": "Make A"}]}


def car(cid, make_id, model_key, mph=100.0):
    return {"Id": cid, "Year": 2001, "MakeID": make_id, "DisplayName": "_&%d" % (0xABC0000 + model_key),
            "MediaName": "MK_Car_%d" % cid, "ClassID": 3, "PerformanceIndex": 0.63896,
            "TopSpeed-mph": mph, "BaseCost": 5000}


def write_pack(root, folder, mod_id, cars, models, makes=(), make_names=None):
    path = os.path.join(root, folder)
    os.makedirs(path)
    with open(os.path.join(path, "mod.json"), "w") as f:
        json.dump({"id": mod_id, "version": "1.2.3", "database": "database.json"}, f)
    tables = {"Data_Car": {"insert": cars}}
    if makes:
        tables["List_CarMake"] = {"insert": list(makes)}
    with open(os.path.join(path, "database.json"), "w") as f:
        json.dump({"tables": tables}, f)
    strings = {"Data_Car": models}
    if make_names:
        strings["List_CarMake"] = make_names
    with open(os.path.join(path, "strings.json"), "w") as f:
        json.dump({"languages": {"EN": strings}}, f)


class ExtractTests(unittest.TestCase):
    def test_cars_named_from_strings_and_makes_from_any_pack(self):
        with tempfile.TemporaryDirectory() as root:
            write_pack(root, "b", "zs-fm6-b", [car(20, 13, 2, 0.0)], {"2": "Second"})
            write_pack(root, "a", "fm5-a", [car(9631, 200, 1)], {"1": "First"})
            write_pack(root, "c", "fm5-common", [], {},
                       makes=[{"ID": 200, "DisplayName": "_&%d" % (0x10000 + 7)}], make_names={"7": "New Make"})
            out = extract(root, BASE)
        cars = out["cars"]
        self.assertEqual([c["id"] for c in cars], [20, 9631])
        self.assertEqual((cars[0]["make"], cars[0]["model"], cars[0]["topSpeedMps"]), ("Make A", "Second", 0.0))
        self.assertEqual(cars[0]["pack"], {"id": "zs-fm6-b", "version": "1.2.3", "game": "FM6"})
        self.assertEqual((cars[1]["make"], cars[1]["model"], cars[1]["pi"]), ("New Make", "First", 639))
        self.assertEqual(cars[1]["topSpeedMps"], 44.704)
        self.assertEqual(cars[1]["pack"]["game"], "FM5")

    def test_r_class_and_out_of_range_pi_become_x_999(self):
        r = car(30, 13, 1)
        r.update({"ClassID": 7, "PerformanceIndex": 0.0})
        hot = car(31, 13, 2)
        hot.update({"ClassID": 6, "PerformanceIndex": 1.162})
        with tempfile.TemporaryDirectory() as root:
            write_pack(root, "a", "fm5-a", [r, hot, car(32, 13, 3)], {"1": "R", "2": "Hot", "3": "A"})
            cars = extract(root, BASE)["cars"]
        self.assertEqual([(c["classId"], c["pi"]) for c in cars], [(6, 999), (6, 999), (3, 639)])
        self.assertEqual(fh2_class(7, 1195), (6, 999))
        self.assertEqual(fh2_class(-1, 500), (6, 999))
        self.assertEqual(fh2_class(6, 999), (6, 999))
        self.assertEqual(fh2_class(0, 1), (0, 1))

    def test_refuses_collisions_and_unresolved_names(self):
        cases = [
            ([car(1, 13, 1)], {"1": "x"}),   # a base car id
            ([car(5, 99, 1)], {"1": "x"}),   # an unknown make
            ([car(5, 13, 1)], {}),           # an unresolved model
        ]
        for cars, models in cases:
            with tempfile.TemporaryDirectory() as root:
                write_pack(root, "a", "fm5-a", cars, models)
                with self.assertRaises(SystemExit):
                    extract(root, BASE)
        with tempfile.TemporaryDirectory() as root:
            write_pack(root, "a", "fm5-a", [car(5, 13, 1)], {"1": "x"})
            write_pack(root, "b", "fm5-b", [car(5, 13, 1)], {"1": "x"})
            with self.assertRaises(SystemExit):
                extract(root, BASE)


if __name__ == "__main__":
    unittest.main()
