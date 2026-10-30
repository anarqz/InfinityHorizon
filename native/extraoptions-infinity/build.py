#!/usr/bin/env python3
"""ExtraOptions.Infinity (OD-125): one winmm.dll for Azimuth 1.0.4 made of ExtraOptions 0.4.4
(native/extraoptions, a submodule pinned to an upstream release), Infinity's patch series
(patches/*.patch, applied to a copy) and the ForzaInfinity hook (native/hook, built with
FI_EO_BUILD and src/core/fi_rename.h).

Usage: python build.py /path/to/zig [--dev] [--build-dir DIR] [--output PATH]

The output goes beside azimuth.exe (Proton needs WINEDLLOVERRIDES=winmm=n,b). The submodule is
never modified; the stock tree builds byte-identical to the upstream release, so the patches
and the hook are the only difference.
"""
import argparse
import importlib.util
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
NATIVE = HERE.parent
EXTRAOPTIONS = NATIVE / "extraoptions"
HOOK = NATIVE / "hook"
# ExtraOptions owns DllMain; the hook starts from its game-map callback (src/core/azimuth_tls.c)
HOOK_EXCLUDED = {"src/core/entry.c"}


def hook_build():
    spec = importlib.util.spec_from_file_location("infinity_hook_build", HOOK / "build.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def extraoptions_version():
    """MOD_VERSION of the submodule's extra_options.h, so the product version follows the pinned release."""
    for line in (EXTRAOPTIONS / "extra_options.h").read_text(encoding="utf-8", errors="replace").splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[:2] == ["#define", "MOD_VERSION"]:
            return parts[2].strip('"')
    raise SystemExit("native/extraoptions/extra_options.h has no MOD_VERSION")


def run(command, **kwargs):
    subprocess.run([str(part) for part in command], check=True, **kwargs)


def prepare_extraoptions(directory):
    tree = directory / "extraoptions"
    shutil.rmtree(tree, ignore_errors=True)
    if not (EXTRAOPTIONS / "build.py").is_file():
        raise SystemExit("native/extraoptions is empty: in the client checkout run "
                         "git submodule update --init native/extraoptions (or clone it with --recursive)")
    shutil.copytree(EXTRAOPTIONS, tree, ignore=shutil.ignore_patterns(".git"))
    # git apply, not GNU patch: Git for Windows ships no patch.exe, and git apply takes no fuzz, so
    # every platform applies the series byte for byte. The ceiling keeps it out of any repository
    # above the copy (inside one it would resolve the paths against that work tree instead).
    # shutil.which honours PATHEXT: a git that is only a git.cmd shim on PATH still runs, which a
    # bare "git" does not (CreateProcess resolves no .cmd).
    git = shutil.which("git")
    if not git:
        raise SystemExit("git is not on PATH: the ExtraOptions.Infinity patches are applied with git apply")
    environment = dict(os.environ, GIT_CEILING_DIRECTORIES=str(tree.parent))
    for patch in sorted((HERE / "patches").glob("*.patch")):
        run([git, "apply", "-p1", "--whitespace=nowarn", patch], cwd=tree, env=environment)
    return tree


def build_hook(hook, zig, directory, options):
    objects_directory = directory / "hook"
    objects_directory.mkdir(parents=True, exist_ok=True)

    class HookOptions:
        dev = options.dev
        diagnostic = False

    arguments = hook.compiler_arguments(zig, HookOptions) + [
        "-DFI_EO_BUILD=1",
        "-include",
        str(HOOK / "src/core/fi_rename.h"),
    ]
    objects = []
    for filename in hook.SOURCES:
        if filename in HOOK_EXCLUDED:
            continue
        target = objects_directory / ("hook_" + Path(filename).stem + ".obj")
        run([*arguments, HOOK / filename, "-o", target])
        objects.append(target)
    library = objects_directory / "hook-kernel32.lib"
    run([zig, "dlltool", "-m", "i386:x86-64", "-d", HOOK / "kernel32.def", "-l", library])
    return objects, library


def build(zig, directory, output, options):
    hook = hook_build()
    wrong = hook.line_ending_errors()
    if wrong:
        raise SystemExit("not CRLF throughout (the hook's sources use CRLF line breaks): " + ", ".join(wrong))
    tree = prepare_extraoptions(directory)
    objects, library = build_hook(hook, zig, directory, options)
    command = [
        sys.executable,
        tree / "build.py",
        zig,
        "--azimuth",
        "--rebuild",
        "--output",
        output,
        "--build-dir",
        directory / "extraoptions-objects",
        "--product-version",
        "ExtraOptions " + extraoptions_version() + " + ForzaInfinity " + hook.mod_version(),
        "--extra-library",
        library,
    ]
    for path in objects:
        command += ["--extra-object", path]
    run(command, cwd=tree)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("zig", type=Path)
    parser.add_argument("--dev", action="store_true", help="the hook's development build (FI_DEV)")
    parser.add_argument("--build-dir", type=Path)
    parser.add_argument("--output", type=Path)
    options = parser.parse_args()
    zig = options.zig.resolve()
    output = (options.output or HERE / "build" / "winmm.dll").resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    if options.build_dir:
        options.build_dir.mkdir(parents=True, exist_ok=True)
        build(zig, options.build_dir.resolve(), output, options)
    else:
        with tempfile.TemporaryDirectory(prefix="extraoptions-infinity-") as temporary:
            build(zig, Path(temporary), output, options)
    print(output)


if __name__ == "__main__":
    main()
