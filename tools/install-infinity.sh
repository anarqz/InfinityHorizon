#!/usr/bin/env bash
# Infinity Horizon installer for Debian and SteamOS (Steam Linux Runtime + Proton).
# curl -fsSL https://raw.githubusercontent.com/anarqz/InfinityHorizon/main/tools/install-infinity.sh | bash
set -euo pipefail

REPO="anarqz/InfinityHorizon"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}/InfinityHorizon"
CONF="$DATA/config"
APP="$DATA/app"
SELF="$DATA/infinity.sh"
DESKTOP="${XDG_DATA_HOME:-$HOME/.local/share}/applications/infinity-horizon.desktop"

c_b=$'\e[1m'; c_g=$'\e[32m'; c_y=$'\e[33m'; c_r=$'\e[31m'; c_0=$'\e[0m'
say()  { printf '%s\n' "$*"; }
ok()   { printf '%s✔%s %s\n' "$c_g" "$c_0" "$*"; }
warn() { printf '%s!%s %s\n' "$c_y" "$c_0" "$*"; }
die()  { printf '%s✘ %s%s\n' "$c_r" "$*" "$c_0" >&2; exit 1; }
ask()  { local a; read -r -p "$1" a </dev/tty || true; printf '%s' "$a"; }
pause() { ask "Press Enter to continue..." >/dev/null; }

save_conf() {
  mkdir -p "$DATA"
  {
    printf 'PROTON=%q\nRUNTIME=%q\nSERVER=%q\nAZIMUTH=%q\n' \
      "$PROTON" "$RUNTIME" "$SERVER" "$AZIMUTH"
  } >"$CONF"
}

check_system() {
  [ "$(uname -s)" = Linux ] || die "Linux only."
  [ "$(uname -m)" = x86_64 ] || die "x86_64 only."
  [ "$(id -u)" -ne 0 ] || die "Run as your normal user, not root."
  for t in curl python3 sha256sum; do
    command -v "$t" >/dev/null || die "Missing '$t'. On Debian: sudo apt install curl python3 coreutils"
  done
  command -v unzip >/dev/null || command -v bsdtar >/dev/null || die "Missing unzip (sudo apt install unzip)."
}

steam_root() {
  local d
  for d in "$HOME/.steam/steam" "$HOME/.local/share/Steam" "$HOME/.steam/debian-installation"; do
    [ -d "$d/steamapps" ] && { readlink -f "$d"; return; }
  done
  [ -d "$HOME/.var/app/com.valvesoftware.Steam" ] && die "Flatpak Steam is not supported yet; install native Steam."
  die "Steam not found. Install Steam, log in once, then rerun."
}

libraries() {
  local root; root=$(steam_root)
  { echo "$root"
    grep -oP '"path"\s*"\K[^"]+' "$root/steamapps/libraryfolders.vdf" 2>/dev/null || true
  } | awk '!seen[$0]++' | while read -r l; do [ -d "$l/steamapps" ] && echo "$l"; done
}

runtime_for() {
  local appid lib dir
  appid=$(grep -oP '"require_tool_appid"\s*"\K[0-9]+' "$1/toolmanifest.vdf" 2>/dev/null) || return 1
  while read -r lib; do
    dir=$(grep -oP '"installdir"\s*"\K[^"]+' "$lib/steamapps/appmanifest_$appid.acf" 2>/dev/null) || continue
    [ -x "$lib/steamapps/common/$dir/run" ] && { echo "$lib/steamapps/common/$dir"; return; }
  done < <(libraries)
  return 1
}

list_protons() {
  local root lib p; root=$(steam_root)
  { while read -r lib; do ls -d "$lib"/steamapps/common/Proton* 2>/dev/null || true; done < <(libraries)
    ls -d "$root"/compatibilitytools.d/* 2>/dev/null || true
  } | while read -r p; do [ -x "$p/proton" ] && echo "$p"; done | sort -rV \
    | awk '/\/Proton [0-9]+\.[0-9]+$/ { print; next } { rest = rest $0 "\n" } END { printf "%s", rest }'
}

pick_proton() {
  local ps=() rs=() p r i=1 n
  say ""; say "${c_b}Proton versions found:${c_0}"
  while read -r p; do
    if r=$(runtime_for "$p"); then
      ps+=("$p"); rs+=("$r")
      printf '  %d) %s  %s(%s)%s\n' "$i" "$(basename "$p")" "$c_g" "$(basename "$r")" "$c_0"; i=$((i+1))
    else
      printf '     %s  %s(runtime missing)%s\n' "$(basename "$p")" "$c_y" "$c_0"
    fi
  done < <(list_protons)
  if [ ${#ps[@]} -eq 0 ]; then
    warn "No Proton with its Steam Linux Runtime installed."
    say "  In Steam: Library > Tools > install 'Proton 11.0' (it pulls 'Steam Linux Runtime 4.0')."
    say "  Or run: steam steam://install/4183110 then a Proton version, and rerun this."
    return 1
  fi
  n=$(ask "Choose [1]: "); n=${n:-1}
  [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -ge 1 ] && [ "$n" -le ${#ps[@]} ] || { warn "Invalid choice."; return 1; }
  PROTON=${ps[$((n-1))]}; RUNTIME=${rs[$((n-1))]}; save_conf
  ok "Using $(basename "$PROTON") with $(basename "$RUNTIME")"
}

ensure_proton() {
  if [ -n "$PROTON" ] && [ -x "$PROTON/proton" ] && [ -x "$RUNTIME/run" ]; then return 0; fi
  PROTON=""; RUNTIME=""
  pick_proton
}

find_azimuth() {
  local lib
  while read -r lib; do
    find "$lib" /run/media "$HOME" -maxdepth 5 -iname 'Azimuth*.exe' -printf '%h\n' 2>/dev/null
  done < <(libraries) | awk '!seen[$0]++' | head -5
}

pick_azimuth() {
  local found=() d i=1 n def=s
  say ""; say "Looking for Azimuth (Forza Horizon 2 PC)..."
  mapfile -t found < <(find_azimuth)
  for d in "${found[@]}"; do printf '  %d) %s\n' "$i" "$d"; i=$((i+1)); done
  say "  m) Type a path    s) Skip (pick it inside the launcher)"
  [ ${#found[@]} -gt 0 ] && def=1
  n=$(ask "Choose [$def]: "); n=${n:-$def}
  case "$n" in
    s) AZIMUTH="" ;;
    m) AZIMUTH=$(ask "Game folder: "); AZIMUTH=${AZIMUTH/#\~/$HOME} ;;
    *) [[ "$n" =~ ^[0-9]+$ ]] && [ "$n" -le ${#found[@]} ] && AZIMUTH=${found[$((n-1))]} || { warn "Invalid choice."; return; } ;;
  esac
  [ -z "$AZIMUTH" ] || [ -d "$AZIMUTH" ] || { warn "Folder not found: $AZIMUTH"; AZIMUTH=""; }
  save_conf; ok "Azimuth: ${AZIMUTH:-chosen in the launcher}"
}

install_app() {
  local tmp desc ver name sha url
  tmp=$(mktemp -d)
  url="https://github.com/$REPO/releases/latest/download"
  say "Checking the latest release..."
  curl -fsSL "$url/infinity-horizon-update-windows-x64.json" -o "$tmp/desc.json" || die "Could not reach GitHub."
  read -r ver name sha < <(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); p=d["package"]; print(d["version"], p["name"], p["sha256"])' "$tmp/desc.json")
  if [ -f "$APP/.version" ] && [ "$(cat "$APP/.version")" = "$ver" ]; then
    rm -rf "$tmp"; ok "Infinity Horizon $ver is already installed."; return
  fi
  say "Downloading Infinity Horizon $ver..."
  curl -fL --progress-bar "https://github.com/$REPO/releases/download/v$ver/$name" -o "$tmp/app.zip" || die "Download failed."
  echo "$sha  $tmp/app.zip" | sha256sum -c --quiet - || die "Checksum mismatch, try again."
  mkdir -p "$tmp/app"
  if command -v unzip >/dev/null; then unzip -q "$tmp/app.zip" -d "$tmp/app"; else bsdtar -xf "$tmp/app.zip" -C "$tmp/app"; fi
  echo "$ver" >"$tmp/app/.version"
  rm -rf "$APP.old"; [ -d "$APP" ] && mv "$APP" "$APP.old"
  mkdir -p "$DATA"; mv "$tmp/app" "$APP"; rm -rf "$APP.old" "$tmp"
  ok "Installed Infinity Horizon $ver"
}

install_shortcuts() {
  mkdir -p "$DATA"
  { echo '#!/usr/bin/env bash'; echo 'set -euo pipefail'
    declare -p REPO DATA CONF APP SELF DESKTOP c_b c_g c_y c_r c_0
    declare -f; echo 'main "$@"'; } >"$SELF.tmp"
  chmod +x "$SELF.tmp"; mv "$SELF.tmp" "$SELF"
  mkdir -p "$(dirname "$DESKTOP")"
  cat >"$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=Infinity Horizon
Comment=Forza Horizon 2 online, through Proton
Exec="$SELF" launch
Icon=$APP/infinity.png
Terminal=false
Categories=Game;
EOF
  ok "Menu shortcut: Infinity Horizon"
}

add_to_steam() {
  install_shortcuts
  if command -v steamos-add-to-steam >/dev/null; then
    steamos-add-to-steam "$DESKTOP" && ok "Added to Steam. It shows up in Game Mode under Non-Steam."
  else
    say "In Steam: Games > Add a Non-Steam Game > Browse, pick:"
    say "  $SELF"
    say "then in its Properties set Launch Options to: launch"
    say "Do NOT force a compatibility tool on it; the script starts Proton itself."
  fi
}

launch() {
  [ -f "$APP/infinity.exe" ] || die "Not installed yet. Run: $SELF"
  ensure_proton || die "Pick a Proton version first."
  pgrep -x infinity.exe >/dev/null && { warn "Infinity Horizon is already running."; return; }
  local root args=() rw="$DATA"
  root=$(steam_root)
  [ -z "$SERVER" ] || args+=(--server "$SERVER" --portal "$SERVER")
  if [ -n "$AZIMUTH" ]; then args+=(--azimuth "Z:${AZIMUTH//\//\\}"); rw="$rw:$AZIMUTH"; fi
  mkdir -p "$DATA/prefix"
  say "Starting Infinity Horizon with $(basename "$PROTON") + $(basename "$RUNTIME")..."
  say "Log: $DATA/launcher.log"
  cd "$APP"
  STEAM_COMPAT_CLIENT_INSTALL_PATH="$root" \
  STEAM_COMPAT_DATA_PATH="$DATA/prefix" \
  STEAM_COMPAT_INSTALL_PATH="$APP" \
  PRESSURE_VESSEL_FILESYSTEMS_RW="$rw" \
  SteamAppId=0 SteamGameId=0 \
    "$RUNTIME/run" -- "$PROTON/proton" waitforexitandrun "$APP/infinity.exe" "${args[@]}" \
    >>"$DATA/launcher.log" 2>&1
}

settings() {
  while true; do
    say ""; say "${c_b}Settings${c_0}"
    say "  1) Proton    ${PROTON:+$(basename "$PROTON")}"
    say "  2) Azimuth   ${AZIMUTH:-auto}"
    say "  3) Server    ${SERVER:-official}"
    say "  0) Back"
    case "$(ask '> ')" in
      1) pick_proton || true ;;
      2) pick_azimuth ;;
      3) SERVER=$(ask "Server URL (empty = official): "); save_conf ;;
      0|"") return ;;
    esac
  done
}

uninstall() {
  [ "$(ask "Remove $DATA (launcher, its Proton prefix, settings)? [y/N] ")" = y ] || return 0
  rm -rf "$DATA" "$DESKTOP"; ok "Removed. Your game files and saves were not touched."; exit 0
}

full_setup() {
  ensure_proton || return 0
  install_app
  [ -n "$AZIMUTH" ] || pick_azimuth
  install_shortcuts
  [ "$(ask "Add Infinity Horizon to Steam? [Y/n] ")" = n ] || add_to_steam
  [ "$(ask "Start it now? [Y/n] ")" = n ] || launch
}

menu() {
  check_system
  say ""; say "${c_b}Infinity Horizon for Linux / SteamOS${c_0}"
  say "Steam: $(steam_root)"
  [ -f "$APP/.version" ] && install_shortcuts >/dev/null
  while true; do
    local inst="not installed"; [ -f "$APP/.version" ] && inst="v$(cat "$APP/.version")"
    say ""
    say "  1) Install / update everything   ($inst)"
    say "  2) Start Infinity Horizon"
    say "  3) Add to Steam"
    say "  4) Settings"
    say "  5) Uninstall"
    say "  0) Quit"
    case "$(ask '> ')" in
      1) full_setup ;;
      2) launch ;;
      3) add_to_steam ;;
      4) settings ;;
      5) uninstall ;;
      0|q) exit 0 ;;
    esac
  done
}

main() {
  SERVER=""
  PROTON=""; RUNTIME=""; AZIMUTH=""
  [ -f "$CONF" ] && . "$CONF"
  case "${1:-}" in
    launch) check_system; launch ;;
    update) check_system; install_app ;;
    *) menu ;;
  esac
}

main "$@"
