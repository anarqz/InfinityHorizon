"""Build the ForzaInfinity hook with Zig 0.15.2, including cross-compilation on Linux.

Derived from the build script of Forza Horizon 2 Extra Options by Arthur (RokkuDayo)
(https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions, 0.2.74 and the version-resource
writer of 0.2.98), GPL-3.0-or-later. See LICENSE and NOTICE.md.

Usage: python build.py /path/to/zig [--dev] [--bridge] [--diagnostic] [--build-dir DIR] [--output PATH]

The output is one freestanding Windows DLL (Kernel32 imports only, no CRT, warnings are errors)
with Azimuth's static-TLS helper interface: a 336-byte zero TLS template and no TLS callbacks.
It is published as infinity.dll and installed as <Azimuth>\\data\\bin\\azimuth-tls.dll, the
file name Azimuth loads (same bytes). --bridge also writes azimuth-tls.dll next to the output,
for the hooks up to 0.2.74+fh2online.58, whose self-update only takes a file of that name.
--azimuth is accepted and ignored (the Azimuth TLS adapter is the only target now).
"""

import argparse
from pathlib import Path
import re
import shutil
import struct
import subprocess
import tempfile

SOURCE = Path(__file__).resolve().parent
SOURCES = [
    # src/core: derived from ExtraOptions' infrastructure (NOTICE.md)
    "src/core/entry.c",
    "src/core/azimuth_tls.c",
    "src/core/support.c",
    "src/core/detour.c",
    "src/core/logging.c",
    "src/core/sha256.c",
    "src/core/azimuth_host.c",
    # src/infinity: ForzaInfinity's own modules
    "src/infinity/runtime.c",
    "src/infinity/mod_host.c",
    "src/infinity/gamer_card.c",
    "src/infinity/voice_ui.c",
    "src/infinity/voice_net.c",
    "src/infinity/activity.c",
    "src/infinity/cfg.c",
    "src/infinity/eo_detect.c",
    "src/infinity/tier.c",
    "src/infinity/livery.c",
    "src/infinity/livery_world.c",
    "src/infinity/game_stream.c",
    "src/infinity/livery_share.c",
    "src/infinity/game_ready.c",
    "src/infinity/offsets.c",
    "src/infinity/net.c",
    "src/infinity/freeroam.c",
    "src/infinity/world.c",
    "src/infinity/proxy.c",
    "src/infinity/udp_proof.c",
    "src/infinity/carstate.c",
    "src/infinity/carstate_hook.c",
    "src/infinity/carmeet.c",
    "src/infinity/follow.c",
    "src/infinity/watch.c",
    "src/infinity/events.c",
    "src/infinity/progress.c",
    "src/infinity/account.c",
    "src/infinity/offline_identity.c",
    "src/infinity/launch.c",
    "src/infinity/codec.c",
    "src/infinity/cloud.c",
    "src/infinity/sign.c",
    "src/infinity/update.c",
    "src/infinity/lang.c",
    "src/infinity/expansions.c",
]
FILE_NAME = "infinity.dll"
BRIDGE_NAME = "azimuth-tls.dll"


def parse_arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("zig", type=Path)
    parser.add_argument(
        "--dev",
        action="store_true",
        help="Development build (FI_DEV): takes the local stack's launch-ticket key and http://127.0.0.1: servers",
    )
    parser.add_argument("--azimuth", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--bridge", action="store_true", help="Also write azimuth-tls.dll (same bytes)")
    parser.add_argument(
        "--diagnostic", action="store_true", help="Exit Azimuth after startup checks"
    )
    parser.add_argument("--output", type=Path)
    parser.add_argument("--build-dir", type=Path)
    arguments = parser.parse_args()
    return arguments


def mod_version():
    header = (SOURCE / "src/core/infinity_base.h").read_text()
    return re.search(r'#define MOD_VERSION "([^"]+)"', header).group(1)


def version_resource(version):
    """A .res file with one VS_VERSIONINFO (for people and tools; the launcher and the old
    self-update read the ASCII "+fh2online.N" marker instead)."""
    # MAJOR.MINOR.PATCH+fh2online.N -> MAJOR.MINOR.PATCH.N (also takes a plain a.b.c[.d])
    match = re.match(r"(\d+)\.(\d+)\.(\d+)(?:\+fh2online\.(\d+)|\.(\d+))?", version)
    numbers = [int(match.group(i) or 0) for i in (1, 2, 3)] + [int(match.group(4) or match.group(5) or 0)]

    def padded(data):
        return data + bytes(-len(data) % 4)

    def wide(text):
        return (text + "\0").encode("utf-16le")

    def block(key, value=b"", children=b"", text=False):
        header = padded(struct.pack("<HHH", 0, len(value) // 2 if text else len(value), int(text)) + wide(key))
        data = header + value
        if children:
            data = padded(data) + children
        return struct.pack("<H", len(data)) + data[2:]

    file_version = ".".join(str(n) for n in numbers)
    strings = {
        "FileDescription": "ForzaInfinity hook (Azimuth TLS adapter)",
        "FileVersion": file_version,
        "InternalName": "infinity",
        "OriginalFilename": FILE_NAME,
        "ProductName": "ForzaInfinity",
        "ProductVersion": version,
        "LegalCopyright": "GPL-3.0-or-later; derived from RokkuDayo's ExtraOptions (see NOTICE.md)",
    }
    entries = b"".join(padded(block(key, wide(value), text=True)) for key, value in strings.items())
    string_info = block("StringFileInfo", children=block("040904b0", children=entries))
    variable_info = block("VarFileInfo", children=block("Translation", struct.pack("<HH", 0x409, 1200)))
    high, low = numbers[0] << 16 | numbers[1], numbers[2] << 16 | numbers[3]
    fixed = struct.pack("<13I", 0xFEEF04BD, 0x10000, high, low, high, low, 0x3F, 0, 0x40004, 2, 0, 0, 0)
    data = block("VS_VERSION_INFO", fixed, padded(string_info) + variable_info)
    null_header = struct.pack("<IIHHHHIHHII", 0, 32, 0xFFFF, 0, 0xFFFF, 0, 0, 0, 0, 0, 0)
    header = struct.pack("<IIHHHHIHHII", len(data), 32, 0xFFFF, 16, 0xFFFF, 1, 0, 0x30, 0x409, 0, 0)
    return null_header + header + padded(data)


def compiler_arguments(compiler, options):
    arguments = [
        str(compiler),
        "clang",
        "-target",
        "x86_64-w64-windows-gnu",
        "-c",
        "-ffreestanding",
        "-fno-builtin",
        "-fno-stack-protector",
        "-funwind-tables",
        "-O2",
        "-Wall",
        "-Wextra",
        "-Werror",
        "-I",
        str(SOURCE),
        "-isystem",
        str(compiler.parent / "lib/include"),
        "-isystem",
        str(compiler.parent / "lib/libc/include/any-windows-any"),
    ]
    if options.dev:
        arguments.append("-DFI_DEV=1")
    if options.diagnostic:
        arguments.append("-DFH2_AZIMUTH_DIAGNOSTIC=1")
    return arguments


def build(compiler, output, directory, options):
    directory.mkdir(parents=True, exist_ok=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    objects = []
    arguments = compiler_arguments(compiler, options)
    for filename in SOURCES:
        object_file = directory / (Path(filename).stem + ".obj")
        subprocess.run([*arguments, str(SOURCE / filename), "-o", str(object_file)], check=True)
        objects.append(str(object_file))
    import_library = directory / "kernel32.lib"
    subprocess.run(
        [
            str(compiler),
            "dlltool",
            "-m",
            "i386:x86-64",
            "-d",
            str(SOURCE / "kernel32.def"),
            "-l",
            str(import_library),
        ],
        check=True,
    )
    resource = directory / "version.res"
    resource.write_bytes(version_resource(mod_version()))
    link_arguments = [
        str(compiler),
        "lld-link",
        "/dll",
        "/entry:DllMain",
        "/machine:x64",
        "/nodefaultlib",
        "/dynamicbase",
        "/nxcompat",
        # reproducible: the same sources give the same bytes (the PE timestamp is a hash)
        "/Brepro",
        "/out:" + str(output),
        "/implib:" + str(directory / "infinity.lib"),
        *objects,
        str(import_library),
        str(resource),
        "/include:_tls_used",
    ]
    subprocess.run(link_arguments, check=True)


def line_ending_errors():
    """C sources, headers and kernel32.def use CRLF line breaks (ExtraOptions' convention, kept):
    every one whose LF count differs from its CRLF count."""
    files = sorted([*SOURCE.glob("src/**/*.[ch]"), SOURCE / "kernel32.def"])
    wrong = []
    for path in files:
        data = path.read_bytes()
        if data.count(b"\n") != data.count(b"\r\n"):
            wrong.append(str(path.relative_to(SOURCE)))
    return wrong


def main():
    options = parse_arguments()
    wrong = line_ending_errors()
    if wrong:
        raise SystemExit("not CRLF throughout (the sources use CRLF line breaks): " + ", ".join(wrong))
    compiler = options.zig.resolve()
    filename = FILE_NAME
    output = (options.output or SOURCE / "build" / filename).resolve()
    if options.build_dir:
        build(compiler, output, options.build_dir.resolve(), options)
    else:
        with tempfile.TemporaryDirectory(prefix="infinity-build-") as temporary:
            build(compiler, output, Path(temporary), options)
    print(output)
    if options.bridge:
        bridge = output.with_name(BRIDGE_NAME)
        if bridge != output:
            shutil.copyfile(output, bridge)
            print(bridge)


if __name__ == "__main__":
    main()
