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
git clone --recursive https://github.com/anarqz/InfinityHorizon.git
cd InfinityHorizon
pwsh -File build.ps1 -Zig C:\tools\zig\zig.exe
```

`--recursive` fetches `native/extraoptions` (ExtraOptions, a submodule the hook is built into); in an
existing clone, run `git submodule update --init native/extraoptions` once. The hook build applies
Infinity's patches to a copy of it with `git apply`, so Git must be on `PATH`.

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

## Saves

Infinity keeps no copy of the game's saves outside the Azimuth folder. Saves stay where Azimuth
writes them:

```text
<Azimuth>\data\titles\1A96545B\saves\player-1\User_<XUID upper hex>\revisions\<16 lowercase hex>\{commit, data\*}
```

- **Profiles.** One list of at most 3 profiles, the same signed in or not. Names are plain
  (letters and digits, no `#NNNNN`). A profile made while signed out (Noob only) plays a local
  folder until the next sign-in, which registers its name and copies its newest sealed revision
  to its account XUID's folder; a taken name asks for a new one, and the profile and its save stay
  as they are. Each profile has its own XUID, so its own `User_<hex>` folder in `player-1`;
  profiles never share a save:
  - **Noob:** one folder, the account XUID's, offline and online: one career.
  - **Hardcore:** played offline, its own local folder (`User_9000000000001` to
    `User_90000000000FF`, never issued by the server), never uploaded; online always continues
    from the cloud save.

  Azimuth's own offline career, `User_9000000000000`, is untouched; a new profile can start
  from it.
- **Revisions.** The `commit` file lists every data file with its SHA-256 and size. A revision
  counts only when it is sealed (every file matches its line). Azimuth loads the highest folder
  and keeps the two newest.
- **Starting from a local save.** A new profile can start from any `User_*` folder with a sealed
  revision (listed most recently played first, with level and credits). The source is only read:
  the picked revision and the target's existing revisions are copied to the archive, a copy is
  staged in `fi-launcher-incoming`, verified against its commit, then renamed into the target's
  `revisions` as the newest one. It runs only while the game is closed, and a crash leaves the
  target as it was.
- **Nothing is deleted.** Whatever Infinity shadows or moves aside (a clone's target, an unsealed
  folder, a cloud install, a conflict) is copied first to `saves\fi-launcher-archive\<User_x>\`.
- **Cloud saves** apply only to profiles whose mode keeps a cloud save line; other profiles are
  local only. Around each play session of such a profile, the launcher holds the line's lease:
  before the game starts it resends any unconfirmed upload, moves unsealed folders to the archive
  and installs a newer cloud revision; a conflict or a folder still being written stops the launch
  instead of guessing. After the game exits, the newest sealed revision is uploaded as an
  `FH2SAVE1` bundle with compare-and-swap on the cloud head. A failed upload stays pending and is
  retried at the next launch.
- **Launcher state** (in the launcher's state folder): `sync-state.json` (each profile's last
  synced revision), `pending\<commitId>.fh2save` (uploads not confirmed yet, resent byte for byte)
  and `pending\set-aside\` (bundles the server refused, kept). A damaged state file is kept aside
  and never causes an upload.
- **Creations** (liveries, thumbnails) are PC-wide files in `player-1`, not per profile, and are
  not synced.

Code: `src/Infinity.Core/Saves/` (`AzimuthSaves`, `CloudSaveFiles`, `CloudSaves`, `SessionSaves`,
`SaveSyncStore`).

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
