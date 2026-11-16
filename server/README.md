# Infinity Horizon server

The server side of Infinity Horizon, and how to run it on your own machine: the account, game and
save APIs (`services/platform`), the migrations and scheduled jobs (`services/cli`), the relay
(`services/relay`) and the server directory (`services/beholder`), with their libraries in
`crates/`. The world engine (lobbies and free-roam worlds) is `../native/worlds`. Licensed under
AGPL-3.0 (`LICENSE`, `NOTICE`).

With this folder you run a complete server on one machine with Docker: the API (every surface on
one port), Postgres, the scheduled jobs and a small world node. Players sign in with the server's
own e-mail and password accounts, saves are stored in a Docker volume, and every key is generated
on the first start. It needs no cloud account and no internet connection after the build. The
released Infinity launcher connects to it as a custom server.

Every command below runs from this folder (`server/` of the clone) unless it says otherwise.

## Contents

1. Prerequisites
2. Game data
3. Configure
4. Start and check
5. World node
6. Point the launcher at your server
7. First account
8. Backup and restore
9. Upgrade
10. Troubleshooting
11. Operator tasks, mail, publishing beyond your LAN, limits

## 1. Prerequisites

- A machine that stays on while people play: Linux, or Windows/macOS with Docker Desktop. 4 GB of
  RAM and about 15 GB of free disk for the first build (the Rust build cache is most of it).
- Docker Engine 26+ with Compose v2.24+ (`docker compose version`).
- Python 3.10+ with `pefile`, for the game-data step only.
- Your own Forza Horizon 2 PC install (the one Infinity plays), and the executable of your Forza
  Motorsport 5 install (`forza_durango_release_final.exe`): the server reads both games'
  web-service schemas, and the build stops without the FM5 one.
- The machine's LAN address (for example `192.0.2.10`; `ip -4 addr` on Linux, `ipconfig` on
  Windows). The game PCs must reach it on TCP `8080` and UDP `5102` (open them in its firewall).
- On every game PC: a released Infinity launcher whose hook build is 89 or newer.

The cargo build is `cargo build --locked --workspace --release` if you want the binaries without
Docker; the toolchain is pinned in `rust-toolchain.toml`.

## 2. Game data

The server ships no game file, and never will: Forza Horizon 2's and Forza Motorsport 5's files
are not ours to distribute. Before it starts, the server needs a game data directory
(`FI_GAME_DATA_DIR`) that you build from your own install. The extractor reads your game files and
writes five files; the 13 tables already in `gamedata/fh2/` (car names, card ids, economy,
gate tables and confirmations, leaderboards, the PlayerDatabase schema, the stats boards, extras
and layout, storefront keywords, top speeds) and the guardrail limits in `gamedata/guard/` are our
own work, published here under the AGPL with the code.

```sh
python3 -m venv .venv && .venv/bin/pip install pefile
FH2="/path/to/Forza Horizon 2"            # the folder holding forza_durango_release_final.exe and media/
.venv/bin/python3 -I tools/gamedata/build.py --out gamedata --fh2 "$FH2" \
  --pt-br "$FH2/media/stripped/stringtables/br.zip" \
  --fm5-exe "/path/to/Forza Motorsport 5/forza_durango_release_final.exe"
```

- `--out gamedata` completes this folder's `gamedata/` in place (its `.gitignore` keeps the
  extracted files and `manifest.json` out of git). Any other directory works too: point
  `FI_GAME_DATA_PATH` at it in `.env`.
- `--pt-br` adds the Brazilian Portuguese car and track names. Without it the catalog is valid but
  differs from the official server's.
- `--packs <folder of installed car packs>` lists your car packs. Without it `fh2/packs.json` is
  `{"cars": []}`, which is fine for a server without car packs.
- The tool ends with `gamedata: complete, manifest written`. If it prints `still missing`, the
  server would refuse to start: fix the input it names. See `tools/gamedata/README.md`.

Never commit or share the built directory: it holds data extracted from the game.

## 3. Configure

```sh
cp .env.example .env
```

Edit `.env` and set this machine's LAN address in two places (here `192.0.2.10` stands for it):

```sh
FI_BIND_ADDRESS=192.0.2.10
FI_PUBLIC_URL=http://192.0.2.10:8080
```

`FI_BIND_ADDRESS` is where the API (TCP `FI_HTTP_PORT`, 8080) and the world node (UDP
`FI_WORLDS_PORT`, 5102) are published; `127.0.0.1`, the default, keeps them on this machine.
`FI_PUBLIC_URL` is the address launchers are told to use; it must be the same address and port.
Every other setting is explained in `.env.example` and can stay at its default. `.env` may hold
secrets later (an SMTP key, a tunnel token): never commit it.

## 4. Start and check

```sh
docker compose up -d --wait
```

The first `up` builds two images (the server and the world node; 10 to 30 minutes). Then, in
order: `keys` writes every missing secret into the `keys` volume (never changing an existing one),
`postgres` initialises its volume, `migrate` applies the migrations and creates the runtime logins
and the pools, then `api` (healthy when `/health/ready` answers), `jobs` and `worlds` start. `keys`
and `migrate` run again on every `up` and exit.

Check what launchers will read (from any PC of the LAN):

```sh
curl -s http://192.0.2.10:8080/launcher/v1/info
```

The answer must have `"apiVersion": 1`, your `realm` (`forzainfinity-selfhost` unless you changed
`FI_REALM`), `"idp": {"kind": "local", ...}` and a `trust` object whose `launchTicket` and
`relayGrant` lists are not empty (`kid:hex` entries). The launcher pins those keys on its first
connection. If `curl` cannot connect, see Troubleshooting.

## 5. World node

The `worlds` service runs `worlds agent`, which supervises the world engine and registers it with
your api. The api trusts only the world nodes listed in `FI_WORLDS`, so registration takes two
steps the first time:

1. Read the node's public key (generated on its first start, kept in the `worlds-key` volume):

   ```sh
   docker compose logs worlds | grep agent.started
   ```

   The line is `{"ev":"agent.started",...,"publicKey":"<44 characters>",...,"vmId":"home-1"}`.

2. Put it in `.env`, with this machine's LAN address and the UDP port, then restart the api:

   ```sh
   FI_WORLDS='[{"vmId":"home-1","publicKey":"<the publicKey>","endpoint":"192.0.2.10:5102","name":"Home","maxWorlds":8}]'
   ```

   ```sh
   docker compose up -d --wait api
   ```

`vmId` and `maxWorlds` must match `FI_WORLDS_VM_ID` and `FI_WORLDS_MAX_WORLDS` (defaults `home-1`
and 8); `name` (1-48 characters) is what players see. Keep the single quotes. Before this step the
agent logs `agent.http.failed` with `"route":"hello","status":401` and retries (at most once a
minute). Within a minute of the api's restart its hello succeeds and it opens its warm worlds:
`docker compose logs worlds` shows `agent.hello` and `agent.world.opened`, and
`docker compose logs api | grep mm_official` shows `mm_official_hello` and `mm_official_world`.

The `endpoint` must be the LAN address, never `127.0.0.1`: the game connects to it through the
hook's proof proxy, which is skipped for loopback addresses, and the engine accepts only proven
traffic. The key never changes unless you delete the `worlds-key` volume; then repeat both steps.

Optional, Road Trip worlds and the Hardcore parts rule: point `FI_WORLDS_CATALOG_DIR` at your
install's `media/stripped` folder and set `FI_WORLDS_CATALOG=/catalog/gamedbRC.slt`, then
`docker compose up -d worlds`.

## 6. Point the launcher at your server

On each game PC, in the Infinity launcher:

1. Next to "Server: official", press **Change**.
2. Choose **Custom server** and type `http://192.0.2.10:8080` (your `FI_PUBLIC_URL`).
3. Press **Save and restart**.

The launcher reads `/launcher/v1/info`, checks the server (its own realm, the local sign-in, the
trust keys) and pins its keys. It warns that the connection is unencrypted (plain http): fine on a
LAN you trust. If the server's keys ever change (a lost `keys` volume), the launcher asks before it
accepts them. Choose the official server again the same way. Official Infinity accounts do not
work on your server, and your server's accounts do not work on the official one.

## 7. First account

With `FI_REGISTRATION_ENABLED=true` (the default) anyone who reaches the server can sign up from
the launcher with an e-mail address and a password. With `FI_MAIL_BACKEND=log` (the default)
nothing is mailed: sign-up works at once and a forgotten password is reset by you (below). Once
your accounts exist, you can close sign-up with `FI_REGISTRATION_ENABLED=false` and
`docker compose up -d`.

Every career keeps working as in the game; Hardcore careers also keep a save line on the server
(their saves are verified and stored in the `blobs` volume). A new Hardcore career starts from the
game's own first save.

## 8. Backup and restore

Back up all four together. The keys hold the database passwords, so a database restored without
its keys cannot be reached, and new keys sign every player out and make launchers warn about
changed keys.

```sh
d=$(date +%F)
docker compose exec -T postgres pg_dumpall -U postgres --roles-only > roles-$d.sql
docker compose exec -T postgres pg_dump -U postgres -Fc forza_infinity > infinity-$d.dump
docker run --rm -v infinity-horizon_blobs:/data -v "$PWD":/out alpine:3.22 tar czf /out/blobs-$d.tgz -C /data .
docker run --rm -v infinity-horizon_keys:/data -v "$PWD":/out alpine:3.22 tar czf /out/keys-$d.tgz -C /data .
docker run --rm -v infinity-horizon_worlds-key:/data -v "$PWD":/out alpine:3.22 tar czf /out/worlds-key-$d.tgz -C /data .
```

The roles go with the dump: `pg_dump` does not carry them, and the migrations that create the
group roles are already recorded as applied in the dump, so a restore without them fails on every
grant. The keys archives hold secrets: store them like a password (encrypted, not next to the dump
on a shared disk). Volume names are `<project>_<volume>`; the project is `infinity-horizon`
(`name:` in compose.yml) unless you pass `-p`.

Restore, into an empty server:

```sh
d=2026-10-25                                   # the backup's date
docker compose down --volumes                  # only if you are replacing this server: deletes its data
docker compose create                          # compose creates the volumes it owns
docker run --rm -v infinity-horizon_keys:/data -v "$PWD":/in alpine:3.22 \
  sh -c 'tar xzf /in/keys-'$d'.tgz -C /data && chown -R 65532:65532 /data && chmod 700 /data && chmod 600 /data/*'
docker run --rm -v infinity-horizon_blobs:/data -v "$PWD":/in alpine:3.22 \
  sh -c 'tar xzf /in/blobs-'$d'.tgz -C /data && chown -R 65532:65532 /data'
docker run --rm -v infinity-horizon_worlds-key:/data -v "$PWD":/in alpine:3.22 \
  sh -c 'tar xzf /in/worlds-key-'$d'.tgz -C /data && chown -R 65532:65532 /data'
docker compose up -d --wait postgres
docker compose exec -T postgres psql -q -U postgres -d postgres < roles-$d.sql   # "already exists" lines for postgres/fi_migrator are expected
docker compose exec -T postgres pg_restore --clean --if-exists -U postgres -d forza_infinity < infinity-$d.dump
docker compose up -d --wait
```

## 9. Upgrade

```sh
git pull --rebase
docker compose up -d --wait --build
```

Take a backup first. `migrate` applies new migrations before the new `api` starts (migrations only
add), and `keys` adds any secret a newer release introduces without touching the existing ones. If
a release adds a game-data file, run the extractor again (section 2); the server names a missing
file when it refuses to start.

## 10. Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `api` exits at start, its log names `manifest.json` or a game-data file | The game data directory is missing or incomplete: run section 2 and check `FI_GAME_DATA_PATH`. |
| `api` exits naming a variable | That setting is invalid; the log says why (for example `FI_WORLDS` JSON or its endpoint, which must be `ip:port`). |
| `curl` to the LAN address cannot connect, `127.0.0.1` works | `FI_BIND_ADDRESS` is still `127.0.0.1`, or the host firewall blocks TCP 8080. Change `.env`, `docker compose up -d`. |
| The launcher says the server did not answer as an Infinity server should | The launcher was pointed at a different origin than `FI_PUBLIC_URL`, `FI_REALM` was set to the official name, or `/info` has no `trust` (compose.yml sets `FI_PUBLISH_TRUST_KEYS=true` on the api: keep it). |
| `worlds` keeps logging `agent.http.failed` with `"route":"hello","status":401` (the api logs `mm_official_auth` refused) | `FI_WORLDS` is empty or its `vmId`/`publicKey` differ from the agent's: repeat section 5 and restart the api. |
| `worlds` keeps logging `agent.http.failed` with `"kind":"network"` | The api is not running, or the pinned network's addresses were changed in only some places (Limits, last item). |
| `worlds` exits with `agent.config.invalid` | The field it names is wrong (for example `FI_WORLDS_CATALOG` names a file that is not mounted). |
| Players are placed in a world but never join it | The `FI_WORLDS` endpoint is `127.0.0.1` or not this machine's LAN address, or UDP 5102 is blocked by the host firewall. |
| The api refuses to start with `FI_MAIL_BACKEND=log` | The server looks public (https or a DNS name in `FI_PUBLIC_URL`, or `trusted-proxy`) with sign-up open: turn mail on or close registration (section 11). |
| The launcher reports that the server's keys changed | The `keys` volume was recreated. Restore it from a backup, or tell players to accept the new keys. |

Logs: `docker compose logs -f api jobs worlds`. They carry ids, never addresses, passwords or
save bytes.

## 11. Operator tasks, mail, publishing beyond your LAN, limits

### Operator tasks

| Task | Command |
| --- | --- |
| Reset a player's password | With mail on, the player uses "Forgot password?". Otherwise `docker compose exec -T api fi-platform accounts reset-password <email>`: prints a temporary password once on stdout, confirms the address and signs the account out everywhere; the player signs in with it and changes it in the launcher. |
| Global online stop | `docker compose exec -T jobs fi-cli online-control status` (`close --reason operator`, `reopen --expected-revision <n>`) |
| Online capacity | `docker compose exec -T jobs fi-cli online-capacity --game fh2 [--set <n>]` |
| A job by hand | `docker compose exec -T jobs fi-cli <command>` (`fi-cli help` lists them) |
| UDP relay | `docker compose --profile relay up -d --wait` (UDP `FI_RELAY_BINDING_PORT` and `FI_RELAY_PORT`). Not needed for the world node, whose players connect directly. For the relay fallback, list it in `FI_RELAYS` (`.env.example`). |
| Stop | `docker compose down` keeps every volume. `docker compose down --volumes` deletes the database, the saves and the keys. |

Use `exec` (not `run`) for these: `run` may rebuild the image and print the build on stdout.

`jobs` runs `fi-cli schedule`: `gate-sweep` every minute; `freeroam-purge` and `saves-audit`
hourly; once a day `consolidate-nightly`, `data-retention`, `content-retention`,
`account-erasure` (accounts whose 14-day deletion grace has passed), `integrity-nightly` (the
Hardcore guardrails over new save revisions; it flags, never refuses), `saves-retention` (each save
line keeps the files of its newest 10 revisions plus the ones the save rules still need) and
`saves-gc` (save files nothing references are deleted a day after they are marked).

### Mail: e-mail confirmation and password reset

With `FI_MAIL_BACKEND=log` nothing is mailed: each mail that would have gone out is one warning in
`docker compose logs api` with its kind and a hash of the address, never the address or link. To
send mail through an SMTP provider (the defaults are Resend's: `smtp.resend.com`, 465, user
`resend`):

1. Add `FI_SMTP_PASSWORD=<the provider's key>` to `.env` (or write it to the keys volume as the
   file `FI_SMTP_PASSWORD`, mode 0600). It is a secret.
2. Set `FI_MAIL_FROM=My Server <no-reply@example.org>` (an address on your verified domain) and
   `FI_MAIL_BACKEND=smtp`; change `FI_SMTP_HOST`, `FI_SMTP_PORT` and `FI_SMTP_USER` for another
   provider.
3. Make sure `FI_PUBLIC_URL` is the address players open links at, then `docker compose up -d`.

With mail on, sign-up mails a confirmation link and "Forgot password?" mails a reset link; both
open a small page on this server.

### Publishing beyond your LAN

The sign-in and sign-up limits count per client network, so the server must know a player's real
address. `FI_CLIENT_ADDRESS` says where it comes from: `peer` (the default, the connection's
address; right on a LAN), or `trusted-proxy` (`CF-Connecting-IP`, else the last
`X-Forwarded-For` entry, believed only from `FI_TRUSTED_PROXIES`). Behind any reverse proxy use
`trusted-proxy` with the proxy's address.

To publish the API through a Cloudflare Tunnel of your own (no open TCP port, TLS at Cloudflare):
create a `cloudflared` tunnel in the Cloudflare dashboard with a public hostname (for example
`infinity.example.com`) whose service is `HTTP` `api:8080`, then set in `.env`:

```sh
CLOUDFLARE_TUNNEL_TOKEN=<the token>
FI_CLIENT_ADDRESS=trusted-proxy
FI_PUBLIC_URL=https://infinity.example.com
```

and run `docker compose --profile tunnel up -d --wait`. `FI_TRUSTED_PROXIES` can stay empty:
the compose then trusts only the `cloudflared` container's fixed address, `172.30.122.10/32`. The
world node's UDP port still has to be reachable directly by players (a tunnel carries only HTTP):
forward UDP `FI_WORLDS_PORT` on your router and use your public address in the `FI_WORLDS`
endpoint.

To list the server in a server directory (beholder) that launchers read, set `FI_BEHOLDER_URL` to
the directory's URL and `FI_ANNOUNCE_NAME`; the api announces itself every 30 s, signed by its own
key in the keys volume. A listing is a hint only: launchers still read `/launcher/v1/info` and pin
the server's keys.

### Limits

- Plain HTTP, bound to `127.0.0.1` unless `FI_BIND_ADDRESS` says otherwise. There is no TLS or
  flood protection: put a TLS reverse proxy or the tunnel profile in front before exposing the API
  beyond a network you trust, and keep Postgres unpublished (it is by default).
- One `keys` volume: every container that mounts it (api, jobs, migrate, postgres read-only,
  relay) can read every key, including the database passwords. Treat a compromised container as a
  compromised server.
- `FI_ENV=prod` refuses this configuration (local sign-in, file save storage): a self-hosted
  server runs with `FI_ENV=local`.
- The pinned network `172.30.122.0/24` gives the api a fixed address for the world node. If it
  clashes with a network of the host, change the subnet and the addresses that use it in
  compose.yml together.
- Not included: the web app, automated backups, several machines, monitoring.
