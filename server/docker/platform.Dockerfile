# syntax=docker/dockerfile:1.27@sha256:bde3983e9c939224420ddaf6b784cc30e09b035a4dea01f581230c50809f372e
#
# The server image: `fi-platform` (the three public APIs, chosen by FI_SURFACES, plus `keys init`,
# `migrate`, `accounts reset-password`, `healthcheck`) and `fi-cli` (the jobs; `fi-cli schedule`
# runs a single-machine server's timetable) from one build. Build context: the Cargo workspace's
# root (platform.Dockerfile.dockerignore keeps it to the workspace).
#
#   docker build -f <this file> -t <tag> <workspace root>
#
# This file is the reference of the configuration: every NON-SECRET setting a server operator may
# change is an ENV below with its default and what it means, and the self-host .env.example lists
# the same names with the same defaults. Secrets (database passwords, signing rings, the pepper)
# are never defaults here: `fi-platform keys init` writes them into the keys volume and every
# process reads a variable it is not given from <FI_KEYS_DIR>/<VARIABLE>. An empty ENV means
# "unset" (the code's default). Precedence: these defaults < the server's `.env` (env_file) < the
# compose's `environment:` (which only wires the server: keys directory, database host, public
# URLs, game data directory).

ARG RUST_IMAGE=rust:1.99-slim-bookworm@sha256:a24e292b4af7bdee351a31d3a6379972a5652f0ea22a1b6d6883324ac4f5097c
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

FROM ${RUST_IMAGE} AS build
# Build-time choice: `release` (default) or `dev`.
ARG PROFILE=release
WORKDIR /repo
COPY . .
RUN --mount=type=cache,id=fi-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=fi-platform-target,target=/repo/target,sharing=locked \
    cargo build --profile "${PROFILE}" --locked --package fi-platform --bin fi-platform \
                --package fi-cli --bin fi-cli \
 && dir=$( [ "${PROFILE}" = dev ] && echo debug || echo "${PROFILE}" ) \
 && install -D "target/${dir}/fi-platform" /out/bin/fi-platform \
 && install -D "target/${dir}/fi-cli" /out/bin/fi-cli \
 && install -d -m 0700 /out/keys \
 && install -d -m 0700 /out/data/blobs

FROM ${RUNTIME_IMAGE} AS runtime
COPY --from=build /out/bin/ /usr/local/bin/
# Mount points owned by the nonroot user: a fresh named volume copies this ownership, so `keys
# init` and the fs blob store can write without root.
COPY --from=build --chown=nonroot:nonroot /out/keys /keys
COPY --from=build --chown=nonroot:nonroot /out/data /data
USER nonroot:nonroot
EXPOSE 8080

# --- Runtime -----------------------------------------------------------------------------------
# Every surface in one process (a standalone instance); a cloud service sets one of infinity,
# game, progression.
ENV FI_SURFACES=all
# The HTTP port inside the container.
ENV PORT=8080
# The private listener (always on): only the play dashboard's reads,
# /internal/dashboard/v1 behind X-Fi-Dashboard-Key = FI_DASHBOARD_KEY (a secret: keys volume or the
# live env file; unset = every dashboard request is 401). Never publish this port.
ENV FI_INTERNAL_BIND=0.0.0.0:8081
# `local` keeps the instance's local defaults; `prod` turns on every strict check and refuses the
# self-host backends (local sign-in, fs blobs) and well-known development keys.
ENV FI_ENV=local
# Log filter, and the format: json (one object per line) or pretty.
ENV RUST_LOG=info
ENV LOG_FORMAT=json
# glibc keeps a freed 6 MB block resident per malloc arena: two arenas bound the save
# verification's memory (measured 2026-10-17). Read by glibc, not by the code.
ENV MALLOC_ARENA_MAX=2

# --- Database ------------------------------------------------------------------------------------
# Connections per process, how long a request waits for one, and the statement cap in ms (empty:
# 5 s for the API, 15 min for jobs, none for migrations).
ENV DB_POOL_MAX=8
ENV DB_ACQUIRE_TIMEOUT_MS=3000
ENV DB_STATEMENT_TIMEOUT_MS=

# --- Identity ------------------------------------------------------------------------------------
# `local`: the built-in e-mail and password sign-in (e-mail confirmation and password reset by mail
# when FI_MAIL_BACKEND=smtp; ACCOUNTS.md §4.1);
# `firebase`: Firebase Authentication / Identity Platform (then FI_IDP_PROJECT_ID, FI_IDP_API_KEY).
ENV FI_AUTH_BACKEND=local
# Accepted sign-in providers (`password`: e-mail and password).
ENV FI_IDP_PROVIDERS=password
# Stable name of the instance in every launcher's state (`/launcher/v1/info` realm).
ENV FI_REALM=forzainfinity-selfhost
# New accounts allowed (`false` closes sign-up and account creation).
ENV FI_REGISTRATION_ENABLED=true
# Multiplier of the sign-in and account-creation limits per client network (raise it when many
# players share one address, e.g. behind one NAT; refused above 1 in prod).
ENV FI_LOCAL_LIMIT_SCALE=1
# Multiplier of the per-account request limits of /launcher/v1 (ACCOUNTS.md §9; refused
# above 1 in prod).
ENV FI_LAUNCHER_ACCOUNT_SCALE=1
# Where the client address (its /24 is the per-network rate-limit subject) comes from:
# `peer` (the TCP peer; forwarding headers ignored), `trusted-proxy` (`CF-Connecting-IP`, else the
# rightmost `X-Forwarded-For`, only from a peer in FI_TRUSTED_PROXIES; a Cloudflare Tunnel) or
# `forwarded` (the rightmost `X-Forwarded-For` from anyone: only behind a front end that sets it).
# Unset in code: `forwarded` locally, `peer` in prod; the image says `peer`.
ENV FI_CLIENT_ADDRESS=peer
# Comma list of networks (a.b.c.d/n) whose requests may name the client (`trusted-proxy`).
ENV FI_TRUSTED_PROXIES=
# `true` publishes the public keys of the launch tickets and relay grants in `/launcher/v1/info`
# (`trust`) for launchers connected to a custom server; `false` keeps the official wire.
# Off by default (a cloud deployable); the self-host compose.yml turns it on for its api.
ENV FI_PUBLISH_TRUST_KEYS=false

# --- Mail (FI_AUTH_BACKEND=local) -----------------------------------------------------------
# `log`: no mail leaves the instance (each would-be mail is one warning line, kind and an address
# hash only) and e-mail confirmation is not required. `smtp`: confirmation and password links by
# mail (required in prod). FI_SMTP_PASSWORD (Resend: the API key) is a secret: set it in .env or
# <FI_KEYS_DIR>/FI_SMTP_PASSWORD, never here. Links use FI_PUBLIC_CORE_URL.
ENV FI_MAIL_BACKEND=log
# SMTP relay: Resend's host, 465 (implicit TLS; 587 is STARTTLS) and user `resend`.
ENV FI_SMTP_HOST=smtp.resend.com
ENV FI_SMTP_PORT=465
ENV FI_SMTP_USER=resend
# Sender, on the provider's verified domain (`Name <address>`); required with smtp.
ENV FI_MAIL_FROM=
# The product name in mail copy (empty: the neutral default "Game Server").
ENV FI_MAIL_PRODUCT=

# --- Public URLs ---------------------------------------------------------------------------------
# The origins `/launcher/v1/info` hands to launchers (http(s)://host[:port], no path). A standalone
# instance serves everything on one origin: the compose sets all four from FI_PUBLIC_URL.
ENV FI_PUBLIC_CORE_URL=
ENV FI_PUBLIC_GATE_URL=
ENV FI_PUBLIC_SAVES_URL=
ENV FI_PUBLIC_MATCHMAKING_URL=

# --- Storage -------------------------------------------------------------------------------------
# `fs`: save blobs in FI_BLOB_DIR (a volume); `firebase`: the Cloud Storage bucket CLOUD_SAVES_BUCKET.
ENV FI_BLOB_BACKEND=fs
ENV FI_BLOB_DIR=/data/blobs

# --- Play ----------------------------------------------------------------------------------------
# Games served.
ENV FI_GAMES=fh2
# The pools (GATE_API.md §2) as JSON: name, tier, capacity, queueLimit, instanceCapacity,
# gameEndpoint. `fi-platform migrate` writes the rows; the compose builds it from FI_PUBLIC_URL.
ENV FI_POOLS_JSON=
# Which pool each tier is admitted to.
ENV FI_POOL_ROUTING=vip=vip,free=matchmaking
# Save lease time-to-live in ms (the launcher heartbeats every fifth of it).
ENV FI_LEASE_TTL_MS=300000
# Game requests' liveness cache, in ms (0 disables).
ENV FI_GAME_LIVENESS_CACHE_MS=60000
# Idle sessions: `true` ends a session idle past the thresholds; `false` only observes.
ENV FI_IDLE_EVICT=false
# Hardcore guardrails: `false` scores and records (shadow), `true` also refuses a score of 60+.
ENV FI_GUARD_ENFORCE=false
# How a Hardcore line starts (SAVES_API.md §4.9): `seed`, `fresh_start` (Azimuth's own first save),
# or empty: `fresh_start` only when the game has no active seed.
ENV FI_HARDCORE_START=
# A Hardcore line seeded from another profile's save, checked on the spot (SAVES_API.md §4.10).
ENV FI_HARDCORE_FROM_PROFILE=false
# Liveries (GAME_PLATFORM.md §3a): distinct bytes per account (25 MiB) and whether other players
# get layered designs (on: the in-game check passed, owner 2026-10-27).
ENV FI_LIVERY_ACCOUNT_BYTES=26214400
ENV FI_LIVERY_LAYERED=true
# Relay fallback (MATCHMAKING_API.md §5b): trusted relays as a JSON array (empty: none, the
# fallback off) and live relayed seats per world.
ENV FI_RELAYS=
# The beholder whose relay list says which trusted relays are down (empty: no check).
ENV FI_RELAY_DIRECTORY_URL=
# Official worlds (WORLDS_OFFICIAL.md): trusted Worlds hosts as a JSON array of
# {vmId, publicKey, endpoint, name, maxWorlds} (empty: none, official worlds off).
ENV FI_WORLDS=
ENV FI_RELAY_SEATS_PER_WORLD=4
# Discord (CORE_API.md §5a): the bot's feed key digests (SHA-256 hex, comma list; empty: the feed
# answers 503) and the bot's public origin for account links (empty: linking unavailable).
ENV FI_FEED_KEYS=
ENV FI_DISCORD_LINK_URL=
# The fresh-start check's ceilings (CLOUD_SAVES.md §7.4): 0 = "zero for everything", but cars 1:
# FH2's opening leaves its intro car in the garage before the first save.
ENV FI_FRESH_START_MAX_CREDITS=0
ENV FI_FRESH_START_MAX_WINNINGS=0
ENV FI_FRESH_START_MAX_XP=0
ENV FI_FRESH_START_MAX_LEVEL=0
ENV FI_FRESH_START_MAX_CARS=1
ENV FI_FRESH_START_MAX_TOKENS=0
# Numbers Azimuth's first save may lack (`winnings`, `tokens`; empty: both required).
ENV FI_FRESH_START_ALLOW_MISSING=
# Version floors and latest versions announced to launchers and hooks (empty: none).
ENV FI_MIN_LAUNCHER=
ENV FI_MIN_HOOK=
ENV FI_LATEST_LAUNCHER=
ENV FI_LATEST_HOOK=

# --- Launcher notices ----------------------------------------------------------------------------
# Message of the day and the support links the launcher shows (empty: none).
ENV FI_MOTD=
ENV FI_SUPPORT_KOFI_URL=
ENV FI_SUPPORT_HELP_URL=
ENV FI_SUPPORT_REPORT_URL=

# --- Beholder (ANNOUNCER_API.md) --------------------------------------------------------
# The announcement directory's base URL (https://beholder.<domain>): set, this process announces
# itself every 30 s, signed by FI_ANNOUNCE_SIGNING_KEY (`keys init` writes it). Empty: off. Only the
# instance behind the public URL sets it.
ENV FI_BEHOLDER_URL=
# The listing's name, 1-48 characters (empty: the endpoint's host).
ENV FI_ANNOUNCE_NAME=
# The origin launchers open, http(s)://host[:port] (empty: FI_PUBLIC_CORE_URL).
ENV FI_ANNOUNCE_ENDPOINT=
# 0-16 of A-Z, a-z, 0-9 and - (for example sa-east; empty: none).
ENV FI_ANNOUNCE_REGION=
# Players admitted at once, 1-1000 (empty: the online capacity, else the pools' sum).
ENV FI_ANNOUNCE_CAPACITY=

# --- Web -----------------------------------------------------------------------------------------
# The web app is off in a standalone instance (Turnstile is deferred while it is), and so is MCP.
ENV FI_WEB_ENABLED=false
ENV FI_MCP_ENABLED=false

ENTRYPOINT ["/usr/local/bin/fi-platform"]
CMD ["serve"]
