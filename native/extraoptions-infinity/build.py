#!/usr/bin/env python3
"""ExtraOptions.Infinity (OD-125): one winmm.dll for Azimuth 1.0.4 made of ExtraOptions 0.6.1
(native/extraoptions, a submodule pinned to upstream main dae2380, the source of the 0.6.1 release's
FH3DLCFix asset, byte-identical; OD-233), Infinity's patch series (patches/*.patch, applied to a copy)
and the ForzaInfinity hook (native/hook, built with FI_EO_BUILD and src/core/fi_rename.h).

The FM5 title profile (upstream fm5/, --fm5) is in the same DLL; see patches/0004. FH3 (--fh3) is
not built. patches/0005 keeps the native route-activation gates in an online session and mirrors the
online feature state into Show All Events' private gates (online Free Roam event markers, 2026-11-10).
patches/0006 sets MOD_VERSION to 0.6.1: upstream built its 0.6.1 release with that bump left
uncommitted (main still says 0.6.0), and the version resource (FILEVERSION 0.6.1.0) and the hook's
ExtraOptions gate (FI_EO_SUPPORTED_MS/LS) follow it. The label is 0.6.1+dae2380 (MOD_VERSION + the pinned
upstream commit): ProductVersion and the hook's FI_EO_SUPPORTED, checked equal here. FM5's
DisableParentalLocks fix (the former 0006) is upstream since 0.6.1.

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


def extraoptions_version(tree):
    """MOD_VERSION of the patched copy's extra_options.h (patches/0006 sets the release's), so the product
    version says what the version resource says."""
    for line in (tree / "shared" / "extra_options.h").read_text(encoding="utf-8", errors="replace").splitlines():
        parts = line.split()
        if len(parts) >= 3 and parts[:2] == ["#define", "MOD_VERSION"]:
            return parts[2].strip('"')
    raise SystemExit("the patched shared/extra_options.h has no MOD_VERSION")


def pinned_commit():
    """The upstream commit native/extraoptions is checked out at (7 hex digits), else the superproject's gitlink."""
    git = shutil.which("git") or "git"
    # rev-parse prints the submodule's commit (the ceiling keeps it from answering with the client's own HEAD
    # when native/extraoptions has no .git); ls-tree prints "160000 commit <commit>\tnative/extraoptions"
    submodule = dict(os.environ, GIT_CEILING_DIRECTORIES=str(NATIVE))
    for command, cwd, field, environment in (([git, "rev-parse", "HEAD"], EXTRAOPTIONS, 0, submodule),
                                             ([git, "ls-tree", "HEAD", "native/extraoptions"], NATIVE.parent, 2, None)):
        try:
            words = subprocess.run(command, cwd=cwd, env=environment, check=True, capture_output=True,
                                   text=True).stdout.split()
        except (OSError, subprocess.CalledProcessError):
            continue
        if len(words) > field and len(words[field]) == 40:
            return words[field][:7]
    raise SystemExit("cannot tell which upstream commit native/extraoptions is (no git checkout)")


def extraoptions_label(tree):
    """The ExtraOptions label (T-1116): MOD_VERSION of the patched copy plus the pinned upstream commit, e.g.
    0.6.1+dae2380. It is the hook's FI_EO_SUPPORTED under FI_EO_BUILD (infinity.h), so both must agree; the
    numeric FILEVERSION (the hook's gate) stays MOD_VERSION's."""
    label = extraoptions_version(tree) + "+" + pinned_commit()
    header = (HOOK / "src" / "infinity" / "infinity.h").read_text(encoding="utf-8", errors="replace")
    block = header[header.index("#ifdef FI_EO_BUILD"):]
    expected = '#define FI_EO_SUPPORTED "' + label + '"'
    if expected not in block[:block.index("#else")]:
        raise SystemExit("native/hook/src/infinity/infinity.h: FI_EO_SUPPORTED (FI_EO_BUILD) must be \"" + label +
                         "\" (MOD_VERSION + the pinned upstream commit); update FI_EO_SUPPORTED, FI_EO_NAME and FI_EO_SHOWN")
    return label


def run(command, **kwargs):
    subprocess.run([str(part) for part in command], check=True, **kwargs)


def prepare_extraoptions(directory):
    tree = directory / "extraoptions"
    shutil.rmtree(tree, ignore_errors=True)
    if not (EXTRAOPTIONS / "build.py").is_file():
        raise SystemExit("native/extraoptions is empty: in the client checkout run "
                         "git submodule update --init native/extraoptions (or clone it with --recursive)")
    # Upstream's release/ (bundled mods with game-derived FM5 textures, AGENTS rule 3) is never copied: the
    # build does not read it, nothing from it may ship, and its deep paths break Windows copies (WinError 206).
    def ignore(directory, names):
        skipped = {".git"}
        if Path(directory).resolve() == EXTRAOPTIONS.resolve():
            skipped.add("release")
        return [name for name in names if name in skipped]

    shutil.copytree(EXTRAOPTIONS, tree, ignore=ignore)
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
        # one winmm.dll for both titles: on_map_game dispatches by the mapped game's title id (patch 0004
        # removes upstream's OfflineVIP and its default-on keys); FH2 never reaches the FM5 code.
        "--fm5",
        "--rebuild",
        "--output",
        output,
        "--build-dir",
        directory / "extraoptions-objects",
        "--product-version",
        "ExtraOptions " + extraoptions_label(tree) + " + ForzaInfinity " + hook.mod_version(),
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
