#!/usr/bin/env python3
"""Extract the Forza.WebServices (Xls, bin/xtsw) type schema from a game executable.

    python3 -I tools/gamedata/extract_schema.py <forza_durango_release_final.exe> <out.json> [--reg 0x...]

Writes the JSON `fi_xtsw::Schema::from_json` reads (game data `fh2/xtsw-schema.json`, or
`fm5/xtsw-schema.json` from Forza Motorsport 5's executable). Needs `pefile` and an unpacked,
decrypted PC executable (the one the operator's own install runs). How it reads the image:

- every type is registered by a four-instruction static initializer
  (`lea rdx, [descriptors]; lea rcx, [cell]; mov r8d, n; jmp <registrar>`); the registrar is the
  jump target most initializers share (`--reg` overrides it);
- the descriptors are n field entries of 0x60 bytes followed by the type's own entry;
- each method has a stub that references the class string "Forza.WebServices.<Class>, ...", the
  method's name and the cells of its _Parameters and _Results types; methods without a stub are
  inferred from the type names.

Nothing of the executable is copied but type, field and method names, kinds and capacities.
"""
import bisect
import collections
import json
import os
import struct
import sys

BASE = 0x140000000
ENTRY = 0x60
NONE = 0x7FFFFFFF

KINDS = {
    0x00: "u8", 0x01: "i16", 0x02: "u16", 0x03: "i32", 0x04: "u32",
    0x05: "i64", 0x06: "u64", 0x07: "f32", 0x09: "string", 0x0C: "struct",
    0x0E: "array", 0x10: "datetime", 0x11: "bool", 0x12: "guid",
    0x13: "f64", 0x14: "blob",
}


class Image:
    def __init__(self, path):
        import pefile  # only the extraction needs it (tests do not)

        pe = pefile.PE(path, fast_load=True)
        self.img = bytes(pe.get_memory_mapped_image())
        self.secs = {s.Name.rstrip(b"\0").decode(): (s.VirtualAddress, s.Misc_VirtualSize)
                     for s in pe.sections}
        va, sz = self.secs[".pdata"]
        funcs = []
        for i in range(0, sz - 12, 12):
            b, e, _ = struct.unpack_from("<III", self.img, va + i)
            if b:
                funcs.append((BASE + b, BASE + e))
        funcs.sort()
        self.funcs = funcs
        self.starts = [f[0] for f in funcs]

    def u64(self, va):
        return struct.unpack_from("<Q", self.img, va - BASE)[0]

    def cstr(self, va):
        o = va - BASE
        return self.img[o:self.img.index(b"\0", o)].decode("latin1")

    def valid(self, va):
        return BASE <= va < BASE + len(self.img)

    def func_of(self, a):
        i = bisect.bisect_right(self.starts, a) - 1
        if i >= 0 and self.funcs[i][0] <= a < self.funcs[i][1]:
            return self.funcs[i]
        return None

    def rip_leas(self):
        """(insn_va, target) of every `lea r64, [rip+disp32]` in .text (a byte scan)."""
        va, sz = self.secs[".text"]
        b = self.img
        out = []
        i = va
        end = va + sz - 7
        find = b.find
        while True:
            i = find(b"\x8d", i + 1, end)
            if i < 0:
                break
            rex = b[i - 1]
            modrm = b[i + 1]
            if rex in (0x48, 0x4C) and (modrm & 0xC7) == 0x05:
                disp = struct.unpack_from("<i", b, i + 2)[0]
                out.append((BASE + i - 1, BASE + i + 6 + disp))
        return out


def initializers(im):
    """Every `lea rdx, [descriptors]; lea rcx, [cell]; mov r8d, n; jmp <target>` in .text:
    [(cell, descriptors, n, target)]."""
    va, sz = im.secs[".text"]
    b = im.img
    out = []
    i = va
    while True:
        i = b.find(b"\x48\x8d\x15", i + 1, va + sz)
        if i < 0:
            break
        if b[i + 7:i + 10] != b"\x48\x8d\x0d" or b[i + 14:i + 16] != b"\x41\xb8" or b[i + 20] != 0xE9:
            continue
        desc = BASE + i + 7 + struct.unpack_from("<i", b, i + 3)[0]
        cell = BASE + i + 14 + struct.unpack_from("<i", b, i + 10)[0]
        n = struct.unpack_from("<I", b, i + 16)[0]
        target = BASE + i + 25 + struct.unpack_from("<i", b, i + 21)[0]
        out.append((cell, desc, n, target))
    return out


def registration_target(found):
    """The type registrar: the most common jump target of the initializers (each type's static
    initializer jumps to the one function that stores {descriptors, n} in its cell)."""
    counts = collections.Counter(t for _, _, _, t in found)
    if not counts:
        raise SystemExit("no type initializer found: not a supported executable")
    return counts.most_common(1)[0][0]


def find_registrations(im, reg=None):
    found = initializers(im)
    reg = registration_target(found) if reg is None else reg
    return {cell: (desc, n) for cell, desc, n, t in found if t == reg}, reg


def entry(im, e):
    q = [im.u64(e + 8 * k) for k in range(12)]
    return {
        "kind": q[0] & 0xFFFFFFFF, "name": im.cstr(q[1]) if im.valid(q[1]) else None,
        "offset": q[2], "cap_off": q[6], "count_off": q[7], "size": q[8],
        "ref": q[9], "elem": q[10], "flags": q[11] & 0xFFFFFFFF,
    }


def build_types(im, regs):
    types = {}
    cell_name = {}
    problems = []
    for cell, (desc, n) in regs.items():
        t = entry(im, desc + n * ENTRY)
        if t["kind"] not in KINDS or t["name"] is None:
            problems.append(hex(cell))
            continue
        cell_name[cell] = t["name"]
    for cell, (desc, n) in regs.items():
        if cell not in cell_name:
            continue
        t = entry(im, desc + n * ENTRY)
        fields = []
        for k in range(n):
            f = entry(im, desc + k * ENTRY)
            fields.append(field_json(im, f, cell_name))
        types[t["name"]] = {"kind": KINDS[t["kind"]], "size": t["size"], "fields": fields}
    return types, cell_name, problems


def type_ref(im, e, cell_name):
    """An array element's type (the entry +0x50 points at)."""
    k = e["kind"]
    d = {"kind": KINDS.get(k, f"unknown_{k:#x}"), "size": e["size"]}
    if k == 0x0C:
        if e["ref"] in cell_name:
            d["type"] = cell_name[e["ref"]]
        else:
            # No registration names this struct: keep the element's own name as a kind the codec
            # refuses (fi-xtsw `the_two_unresolved_elements_are_refused`), never an invented type.
            d["kind"] = f"unresolved_{e['name']}"
    return d


def field_json(im, f, cell_name):
    k = f["kind"]
    d = {"name": f["name"], "kind": KINDS.get(k, f"unknown_{k:#x}"), "offset": f["offset"],
         "size": f["size"]}
    if f["flags"]:
        d["flags"] = f["flags"]
    if k == 0x0C:
        d["type"] = cell_name.get(f["ref"], f"?{f['ref']:#x}")
    elif k == 0x09:
        # fixed capacity in WCHARs, terminator included
        if f["cap_off"] == NONE:
            d["max_chars"] = f["size"] // 2 - 1 if f["size"] else None
    elif k in (0x0E, 0x14):
        if k == 0x0E:
            d["elem"] = type_ref(im, entry(im, f["elem"]), cell_name)
        if f["cap_off"] == NONE and f["size"]:
            if k == 0x0E:
                es = d["elem"]["size"]
                d["max_count"] = f["size"] // es if es else None
            else:
                d["max_len"] = f["size"]
    return d


def build_services(im, cell_name):
    leas = im.rip_leas()
    by_func = {}
    for a, t in leas:
        f = im.func_of(a)
        if f:
            by_func.setdefault(f[0], []).append((a, t))
    services = {}
    unmatched = []
    for fstart, refs in by_func.items():
        strs, cells = [], []
        for a, t in sorted(refs):
            if t in cell_name:
                cells.append(cell_name[t])
            elif im.valid(t):
                try:
                    s = im.cstr(t)
                except ValueError:
                    continue
                if s.isprintable() and len(s) < 120:
                    strs.append(s)
        cls = [s for s in strs if s.endswith(", Forza.WebServices") or s.endswith(", Xls.WebServices")]
        if len(cls) != 1:
            continue
        params = [c for c in cells if c.endswith("_Parameters")]
        results = [c for c in cells if c.endswith("_Results")]
        methods = [s for s in strs if s != cls[0] and s.isidentifier()]
        full = cls[0]
        key = full.split(",")[0]
        short = key.split(".")[-1]
        # the method is the identifier that matches a type name, if any
        meth = None
        for m in methods:
            for p in params + results:
                if p.endswith(short + m + "_Parameters") or p.endswith(short + m + "_Results"):
                    meth = m
        if meth is None and len(methods) == 1:
            meth = methods[0]
        if meth is None:
            unmatched.append({"func": hex(fstart), "class": full, "strings": methods, "cells": cells})
            continue
        svc = services.setdefault(key, {"className": full, "methods": {}})
        svc["methods"][meth] = {
            "source": "stub " + hex(fstart),
            "params": params[0] if params else None,
            "results": results[0] if results else None,
        }
    return services, unmatched


def class_strings(im):
    """Every "<Ns>.WebServices.<Class>, <Ns>.WebServices" string in .rdata."""
    va, sz = im.secs[".rdata"]
    b = im.img
    out = set()
    for suffix in (b", Forza.WebServices\0", b", Xls.WebServices\0"):
        i = va
        while True:
            i = b.find(suffix, i + 1, va + sz)
            if i < 0:
                break
            s = b.rfind(b"\0", va, i) + 1
            out.add(b[s:i + len(suffix) - 1].decode("latin1"))
    return out


def infer_by_name(services, types, classes):
    """Completes the map from type names: <prefix><Class><Method>_Parameters/_Results.

    The prefix varies (Forza, ForzaWebServices, WebServices or none); when one short name exists
    in both Forza.* and Xls.* (User, Club...), the "Forza" prefix means Forza.WebServices.
    """
    known = {(t) for s in services.values() for m in s["methods"].values()
             for t in (m["params"], m["results"]) if t}
    by_short = {}
    for full in classes:
        key = full.split(",")[0]
        by_short.setdefault(key.split(".")[-1], []).append((key, full))
    for t in sorted(types):
        if t in known:
            continue
        for suf, slot in (("_Parameters", "params"), ("_Results", "results")):
            if not t.endswith(suf):
                continue
            stem = t[:-len(suf)]
            best = None
            for prefix in ("ForzaWebServices", "Forza", "WebServices", ""):
                if not stem.startswith(prefix):
                    continue
                rest = stem[len(prefix):]
                for short, cands in by_short.items():
                    if rest.startswith(short) and len(rest) > len(short) and rest[len(short)].isupper():
                        if best is None or len(short) > len(best[1]):
                            best = (prefix, short, rest[len(short):], cands)
                if best:
                    break
            if not best:
                continue
            prefix, short, meth, cands = best
            ns = "Forza" if prefix.startswith("Forza") else "Xls"
            pick = [c for c in cands if c[0].startswith(ns)] or cands
            key, full = pick[0]
            svc = services.setdefault(key, {"className": full, "methods": {}})
            m = svc["methods"].setdefault(meth, {"source": "name", "params": None, "results": None})
            if m[slot] is None:
                m[slot] = t


def schema(im, source, reg=None):
    regs, reg = find_registrations(im, reg)
    types, cell_name, problems = build_types(im, regs)
    services, unmatched = build_services(im, cell_name)
    infer_by_name(services, types, class_strings(im))
    for s in services.values():
        s["methods"] = dict(sorted(s["methods"].items()))
    return {
        "source": source,
        "kinds": {f"{k:#x}": v for k, v in KINDS.items()},
        "services": dict(sorted(services.items())),
        "types": dict(sorted(types.items())),
        "unresolved_registrations": problems,
        "unmatched_stubs": unmatched,
    }, reg, len(regs)


def main(argv):
    args = list(argv[1:])
    reg = None
    if "--reg" in args:
        i = args.index("--reg")
        reg = int(args[i + 1], 16)
        del args[i:i + 2]
    if len(args) != 2:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    exe, out_path = args
    out, reg, n = schema(Image(exe), os.path.basename(exe), reg)
    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(out, f, indent=1)
    methods = sum(len(s["methods"]) for s in out["services"].values())
    print(f"registrar={reg:#x} types={len(out['types'])} registrations={n} "
          f"untyped={len(out['unresolved_registrations'])} classes={len(out['services'])} "
          f"methods={methods} unmatched_stubs={len(out['unmatched_stubs'])}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
