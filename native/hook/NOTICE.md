# NOTICE

The ForzaInfinity hook is free software under the **GNU General Public License, version 3 or
later** (`LICENSE`).

It started as a fork of **Forza Horizon 2 Extra Options** by **Arthur (RokkuDayo)**,
<https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions>, also GPL-3.0. ForzaInfinity's own
code was written on top of Extra Options 0.2.74 (upstream commit `c2e3298`) as the `fh2online`
branch of the fork (`anarqz/ForzaHorizon2_ExtraOptions`, last shipped as `0.2.74+fh2online.58`,
commit `cd4d807`). Since `0.3.0+fh2online.59` the hook contains **none of Extra Options'
features**: players who want them install Extra Options themselves (it is supported next to the
hook in version 0.2.98, on Azimuth 0.9.10), and the hook never modifies, ships or
redistributes it.

## Files derived from Extra Options

These files keep Extra Options' infrastructure (Azimuth's static-TLS helper interface, finding
the mapped game image, small support routines, the log, SHA-256, guarded host patches, the
build script). Each one carries a notice crediting Arthur (RokkuDayo).

| File here | Origin in Extra Options (`c2e3298`, 0.2.74) | What changed |
| --- | --- | --- |
| `src/core/entry.c` | `dllmain.c` | `InfinityStatus` instead of `Fh2TweaksStatus`; calls `fi_process_attach` |
| `src/core/azimuth_tls.c` | `adapters/azimuth.c` | no options or patch engine; the module is pinned; the Azimuth root path is kept |
| `src/core/support.c`, `support.h` | `support.c`, `support.h` | header name only |
| `src/core/logging.c`, `logging.h` | `logging.c`, `logging.h` | no option dump; `infinity.log` with `.previous` rotation; `[hook] Log` |
| `src/core/sha256.c`, `sha256.h` | `sha256.c`, `sha256.h` | header name only |
| `src/core/azimuth_host.c`, `azimuth_host.h` | `adapters/azimuth_host.c`, `adapters/azimuth_host.h` | the 0.9.10 build entry (fork); the module API moved to `src/infinity/infinity.h` |
| `src/core/infinity_base.h` | `extra_options.h` | only the shared constants; new names |
| `src/core/runtime.h` | `runtime.h` | the two entry points of the new runtime |
| `src/infinity/livery.c` | `adapters/azimuth_livery.c` | the 2x MSAA mask patch for Azimuth 0.9.6 to 0.9.9 (ExtraOptions' own, kept as it was; ExtraOptions 0.2.98 never runs on those builds) and the fork's placement-texture code |
| `src/infinity/carstate_hook.c` | `features/vehicle_controls.c`, `features/autodrive_game.c` | the local car, a car's animation and its events, as Extra Options reads them; the detours and the remote-car code are ForzaInfinity's |
| `build.py` | `build.py` (and the version-resource writer of 0.2.98's `build.py`) | one target, new source list, `/Brepro`, `--bridge` |
| `kernel32.def` | `kernel32.def` | more Kernel32 functions |
| `LICENSE` | `LICENSE` | unchanged (GPL-3.0 text) |

The remaining implementation in `src/infinity/` is ForzaInfinity's own code, written for the
`fh2online` branch and this repository, and is GPL-3.0-or-later as part of this work.
`src/infinity/sign.c` uses the verification path of TweetNaCl (public domain).

## Not included

- No file of Forza Horizon 2, of Azimuth or of Extra Options' releases is part of this
  repository or of its build output. The hook checks a few short byte patterns of the game and
  of `azimuth.exe` before it changes them, as any patch must; the tests read the game's and
  Azimuth's files from a player's own install, at run time, and never copy them.
- Compatibility checks and real-install tests are maintained separately; they are not needed to build this source.
