# Infinity Launcher

The client that runs on players' computers: a C# WPF launcher with its integrated world relay,
a C hook and a C++ Direct3D 12 overlay. Windows x64; the Windows build also runs through
Wine/Proton. No game, Azimuth executable, player save or backend service is included.

## Install on Linux and Steam Deck

Debian and SteamOS, with native Steam, a Proton version and its Steam Linux Runtime installed
(Steam > Library > Tools: Proton 11.0 pulls Steam Linux Runtime 4.0). Run in a terminal
(Konsole in the Deck's Desktop Mode):

```sh
curl -fsSL https://raw.githubusercontent.com/anarqz/InfinityHorizon/main/tools/install-infinity.sh | bash
```

The menu installs or updates the latest release, picks Proton, finds the Azimuth folder, adds a
menu entry and a Steam shortcut, and starts the launcher. Run it again to update, change settings
or uninstall; `~/.local/share/InfinityHorizon/infinity.sh launch` starts the launcher directly.
Everything lives in `~/.local/share/InfinityHorizon`, including the launcher's Proton prefix.

## Build

On Windows, install PowerShell 7, .NET SDK 10.0.401 (or a newer patch in that feature band),
Visual Studio C++ build tools with a Windows SDK, Python 3.10+ and Zig 0.15.2.
NuGet restores the pinned BouncyCastle package; ImGui, MinHook and the embedded fonts are vendored.

```powershell
git clone https://github.com/anarqz/InfinityHorizon.git
cd InfinityHorizon
pwsh -File build.ps1 -Zig C:\tools\zig\zig.exe
```

The output is `artifacts/win-x64/`: `infinity.exe` and `infinity-updater.exe` (self-contained .NET),
`hook/infinity.dll`, `overlay/infinity-overlay.dll`, their development signatures,
English/pt-BR catalogs and license notices. No private repository or game installation is needed
to compile. Build just the managed projects with `dotnet build Infinity.sln -c Release`.

## Run a local build

```powershell
pwsh -File run.ps1 -Server http://127.0.0.1:8480 -Portal http://127.0.0.1:3120
```

The launcher opens even when no server is available. Sign-in and online play require a compatible
backend and the player's own game/Azimuth installation. `--server` / `FI_SERVER` and
`--portal` / `FI_PORTAL` select a deployment; the defaults are loopback development addresses.
Development signature trust is accepted only with development origins.
Development builds keep development as their default. `--environment production` (or `FI_ENVIRONMENT=production`)
explicitly selects the official HTTPS bootstrap and isolated local state; it never restores a
development session. Production discovery validates each service origin and the native Identity
Platform project, with no emulator or fallback to LAN. `run.ps1` always selects development.
The local signing seed stays in the user's LocalAppData, outside this checkout; the build stores
only its public verification key in the output. Starting the launcher does not start a game.

`build.ps1 -Zig <path> -Version <semver> -Production` compiles without development trust and leaves all four binaries
unsigned. It never uses an official private key. Maintainers sign official distribution artifacts
separately. Production launch-ticket and relay public trust are embedded separately from the
release/update signing key; development builds of the hook keep only local ticket trust.
Binary builds belong in versioned releases, rather than in the source tree.

`-Production` uses a separate `artifacts/production/win-x64` output and compiles the official
production-only profile: launching without arguments uses the official HTTPS bootstrap and
isolated production state. Ambient development endpoint variables are ignored and development
CLI selections are refused. `-Output <new-empty-directory>` selects an isolated staging path;
production builds cannot overwrite the development runtime. Local ticket/relay trust is omitted
from the production managed binaries. Maintainers still sign all production bytes separately.

The launcher checks its executable, bundled hook/overlay and any installed Infinity hook against
the official release public key. Keep each detached `.sig` beside its binary. Missing, modified
or development-signed files show a caution with a link to
[official releases](https://github.com/anarqz/InfinityHorizon/releases). This advisory does not add
a play restriction; the existing hook/overlay signature checks still apply. The caution is
expected when building from source. A binary's signature proves its signed bytes, not that a
running client is unmodified.

At startup, the launcher checks the latest stable release and backend version policy. Online
play requires reachable backend version policy and a compatible version. Unavailable release
discovery is advisory; a known required update has its own screen;
account and support actions remain accessible. Compatible newer builds from source are not
downgraded. Version compatibility is separate from the build-origin advisory above.

Official updates authenticate a signed descriptor and every runtime file before a separate
helper replaces the launcher after it exits. The helper verifies the package again, keeps a
rollback journal and waits for the replacement to initialize before committing. Updates wait
until the game and save cleanup have finished. Keep the launcher in its own directory, outside
the Azimuth game directory, so runtime updates can be applied there.

## License

Infinity Launcher is free software, licensed under the
[GNU General Public License version 3](LICENSE) (GPL-3.0-only) with one additional term under its
section 7(b), stated in [NOTICE](NOTICE).

**Forks and derived works must credit this project.** They must keep the following attribution,
unaltered, in the legal notices their interface displays (its start screen, main window or an
"About" screen reachable from it):

> Based on Infinity Launcher - https://github.com/anarqz/InfinityHorizon

Under the GPL, anyone who distributes a modified version must also publish its full source code
under the same license and terms.
