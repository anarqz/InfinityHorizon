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
NuGet restores the pinned BouncyCastle package; ImGui, MinHook, SQLite and the embedded fonts are vendored.

```powershell
git clone https://github.com/anarqz/InfinityHorizon.git
cd InfinityHorizon
pwsh -File build.ps1 -Zig C:\tools\zig\zig.exe
```

The output is `artifacts/win-x64/`: `infinity.exe` and `infinity-updater.exe` (self-contained .NET),
`hook/infinity.dll`, `overlay/infinity-overlay.dll`, `mod/infinity_mod.dll`,
`mod/infinity-mod-prepare.exe`, their development signatures,
English/pt-BR catalogs and license notices. No private repository or game installation is needed
to compile. Build just the managed projects with `dotnet build Infinity.sln -c Release`.

## Run a local build

```powershell
pwsh -File run.ps1 -Server http://127.0.0.1:8480 -Portal http://127.0.0.1:3120
```

Local builds default to development. Online play requires a compatible backend and your own
game/Azimuth installation. Starting the launcher does not start the game. Keep the launcher
in its own directory, separate from Azimuth, and retain detached `.sig` files beside binaries.
Development builds show an expected unverified-origin advisory.

`build.ps1 -Zig <path> -Version <semver> -Production` builds unsigned production binaries in
`artifacts/production/win-x64`, with official endpoints and no development trust. Official
signing and publication are separate maintainer operations; building does not create a release.

## Native mods (development)

The launcher reads mod packages, dependencies and settings from manifests. Compatible native
mods use the MIT headers in `native/mod/include/`. The development runtime supports offline
initialization and startup SQLite sessions with launcher-managed restoration. Production
catalog admission, live database updates and online lobby enforcement are not available.

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
