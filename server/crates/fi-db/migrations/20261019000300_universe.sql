-- Platform v1, lane saves-mm (ADR 0016, ADR 0017, ADR 0902; docs/contracts/UNIVERSE_API.md,
-- MATCHMAKING_API.md): the Supporters' universe serving tables, the matchmaking control plane,
-- the gold snapshot core serves, and `game` on game_presence.
--
-- Every per-game table carries `game` as the first column of its keys and indexes (ADR 0902 §1).
-- The serving tables are read models the game overwrites (history is the lake's): one row per
-- profile per board, per event (ghost), per game (position). Writers are the progress handler of
-- fi-saves and the matchmaking service, through fi_db::{universe, matchmaking}; nothing here is
-- written by a job except gold_snapshot (fi-cli gold-build) and the sweeps.

-- 1. game_presence.game: the game a session's presence was served for (the profile's).
ALTER TABLE game_presence ADD COLUMN game text NOT NULL DEFAULT 'fh2' REFERENCES game (code);
ALTER TABLE game_presence ALTER COLUMN game DROP DEFAULT;
CREATE INDEX game_presence_game_seen ON game_presence (game, last_seen_at);

-- A writer that names no game gets its profile's (the column is NOT NULL, checked after this).
CREATE FUNCTION fi_game_presence_game() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.game IS NULL THEN
    SELECT game INTO NEW.game FROM profile WHERE id = NEW.profile_id;
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER game_presence_game BEFORE INSERT ON game_presence FOR EACH ROW EXECUTE FUNCTION fi_game_presence_game();

-- 2. Progress facts of kind `ghost` are kept like the other kinds (UNIVERSE_API.md §3.1);
-- `position` facts are never progression events (they only move player_position).
ALTER TABLE progression_event DROP CONSTRAINT progression_event_kind_check;
ALTER TABLE progression_event ADD CONSTRAINT progression_event_kind_check
  CHECK (kind IN ('profile_sync', 'tables_sync', 'score', 'xp', 'race_result', 'drivatar', 'livery', 'ghost'));

-- 3. Leaderboards. `order_key` sorts best first in ascending order for both units (times: the
-- value; points: minus the value), so one ascending index serves the top page, the keyset cursor
-- `(order_key, achieved_at, profile_id) > (...)`, the caller's rank (an index range count) and the
-- rows around the caller (a backward scan). An entry moves only when a fact is better (strictly
-- smaller order_key): an older fact arriving late can never lower it.
CREATE TABLE board_entry (
  game        text NOT NULL REFERENCES game (code),
  mode        text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  board       text NOT NULL CHECK (board ~ '^[A-Za-z0-9][A-Za-z0-9_.:-]{0,63}$'),
  scope       text NOT NULL CHECK (scope IN ('global', 'friends', 'club')),
  profile_id  uuid NOT NULL REFERENCES profile (id),
  unit        text NOT NULL CHECK (unit IN ('ms', 'points')),
  value       bigint NOT NULL,
  order_key   bigint NOT NULL,
  achieved_at timestamptz NOT NULL,
  -- The fact that set the value (progression_event.event_id).
  event_id    uuid NOT NULL,
  car         integer CHECK (car IS NULL OR car >= 0),
  -- Improvements so far (1 on insert).
  revision    bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
  updated_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, mode, board, scope, profile_id),
  CONSTRAINT board_entry_order_key CHECK (order_key = CASE unit WHEN 'ms' THEN value ELSE -value END),
  CONSTRAINT board_entry_time_positive CHECK (unit <> 'ms' OR value > 0)
);

CREATE INDEX board_entry_rank ON board_entry (game, mode, board, scope, order_key, achieved_at, profile_id);

-- A value a plausibility rule refused once per line (CR-L14 rule 4): the same value, or a less
-- extreme one, is never refused by that rule again; only a more extreme one is a new refusal.
CREATE TABLE universe_refusal (
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  board      text NOT NULL,
  rule       text NOT NULL CHECK (rule IN ('min_time', 'max_points')),
  value      bigint NOT NULL,
  event_id   uuid NOT NULL,
  refused_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id, board, rule)
);

-- 4. Ghosts. The recording is a create-only object in the cloud-saves bucket under its SHA-256
-- (the save-blob rules, CLOUD_SAVES.md §6); ghost_blob says it exists (PUT answers 201 once, then
-- 200) and is the reference saves-gc must honour before deleting such an object. One ghost per
-- profile per event: the fastest.
CREATE TABLE ghost_blob (
  sha256      bytea PRIMARY KEY CHECK (length(sha256) = 32),
  game        text NOT NULL REFERENCES game (code),
  size        integer NOT NULL CHECK (size BETWEEN 1 AND 262144),
  uploaded_by uuid NOT NULL REFERENCES profile (id),
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE ghost (
  game        text NOT NULL REFERENCES game (code),
  mode        text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  event       text NOT NULL CHECK (event ~ '^[A-Za-z0-9][A-Za-z0-9_.:-]{0,63}$'),
  profile_id  uuid NOT NULL REFERENCES profile (id),
  time_ms     bigint NOT NULL CHECK (time_ms > 0),
  blob_sha256 bytea NOT NULL REFERENCES ghost_blob (sha256),
  size        integer NOT NULL CHECK (size BETWEEN 1 AND 262144),
  event_id    uuid NOT NULL,
  recorded_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, mode, event, profile_id)
);

CREATE INDEX ghost_by_time ON ghost (game, mode, event, time_ms, profile_id);
CREATE INDEX ghost_by_blob ON ghost (blob_sha256, game, mode);

-- 5. Positions for "players near me". `cell` packs the integer grid cell (x >> 10, y >> 10) of
-- the map plane (fi_db::universe::cell); a nearby read selects the cells its radius's bounding
-- square touches (the caller's cell and its neighbours) and filters the exact distance in SQL on
-- those rows only, newest within 2 minutes. Never a scan: the index leads with the cell.
CREATE TABLE player_position (
  game          text NOT NULL REFERENCES game (code),
  profile_id    uuid NOT NULL REFERENCES profile (id),
  mode          text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  cell          bigint NOT NULL,
  x             integer NOT NULL CHECK (x BETWEEN -1000000 AND 1000000),
  y             integer NOT NULL CHECK (y BETWEEN -1000000 AND 1000000),
  heading       smallint CHECK (heading BETWEEN 0 AND 359),
  car           integer CHECK (car IS NULL OR car >= 0),
  session_id    uuid NOT NULL REFERENCES play_session (id),
  -- The position fact that wrote the row: its replay answers recorded: false.
  last_event_id uuid NOT NULL,
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id)
);

CREATE INDEX player_position_cell ON player_position (game, mode, cell, updated_at) INCLUDE (profile_id, x, y);

-- 6. Worlds (player-hosted, announced every 15 s, alive 45 s past the last announce). The host's
-- public endpoint is stored for the rendezvous and never listed. One open world per host profile.
CREATE TABLE world (
  id              uuid PRIMARY KEY,
  game            text NOT NULL REFERENCES game (code),
  host_profile_id uuid NOT NULL REFERENCES profile (id),
  host_account_id uuid NOT NULL REFERENCES account (id),
  host_session_id uuid NOT NULL REFERENCES play_session (id),
  host_tier       text NOT NULL CHECK (host_tier IN ('free', 'vip')),
  kind            text NOT NULL CHECK (kind IN ('free', 'supporter')),
  name            text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 48),
  mode            text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  players         integer NOT NULL DEFAULT 0 CHECK (players BETWEEN 0 AND 12),
  peak_players    integer NOT NULL DEFAULT 0 CHECK (peak_players BETWEEN 0 AND 12),
  max_players     integer NOT NULL CHECK (max_players BETWEEN 1 AND 12),
  -- HMAC-SHA256(key = the world id's 16 bytes, password); NULL: no password.
  password_hash   bytea CHECK (password_hash IS NULL OR length(password_hash) = 32),
  races           boolean NOT NULL,
  message         text CHECK (char_length(message) <= 140),
  visibility      text NOT NULL CHECK (visibility IN ('public', 'friends', 'secret')),
  endpoint_ip     inet NOT NULL,
  endpoint_port   integer NOT NULL CHECK (endpoint_port BETWEEN 1 AND 65535),
  peers           bigint[] NOT NULL DEFAULT '{}',
  -- Supporter hosts only: who may join (NULL: anyone the bans let in).
  allow           bigint[],
  -- Milliseconds: the list's keyset cursor carries them exactly.
  opened_at       timestamptz NOT NULL DEFAULT date_trunc('milliseconds', clock_timestamp()),
  announced_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at      timestamptz NOT NULL,
  closed_at       timestamptz,
  close_reason    text CHECK (close_reason IN ('host_closed', 'expired', 'replaced')),
  CONSTRAINT world_closed_shape CHECK ((closed_at IS NULL) = (close_reason IS NULL)),
  CONSTRAINT world_kind_follows_tier CHECK ((kind = 'supporter') = (host_tier = 'vip')),
  CONSTRAINT world_allow_supporter_only CHECK (allow IS NULL OR kind = 'supporter')
);

CREATE UNIQUE INDEX world_one_open_per_host ON world (game, host_profile_id) WHERE closed_at IS NULL;
CREATE INDEX world_list ON world (game, opened_at DESC, id) WHERE closed_at IS NULL AND visibility = 'public';
CREATE INDEX world_open_expiry ON world (expires_at) WHERE closed_at IS NULL;

-- A Supporter host's bans, kept per host profile: they apply to every future world of that host.
CREATE TABLE world_ban (
  game              text NOT NULL REFERENCES game (code),
  host_profile_id   uuid NOT NULL REFERENCES profile (id),
  target_profile_id uuid NOT NULL REFERENCES profile (id),
  until             timestamptz,
  reason            text CHECK (char_length(reason) <= 200),
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, host_profile_id, target_profile_id)
);

CREATE INDEX world_ban_target ON world_ban (game, target_profile_id);

-- Join grants: a seat reserved for 60 s (`issued`), then `used` while the host reports the peer,
-- and one terminal outcome (`released`, `expired`, `kicked`), announced once as join.granted. At
-- most one live grant per profile and game: the lock of OD-25.
CREATE TABLE mm_grant (
  id           uuid PRIMARY KEY,
  game         text NOT NULL REFERENCES game (code),
  world_id     uuid NOT NULL REFERENCES world (id),
  profile_id   uuid NOT NULL REFERENCES profile (id),
  account_id   uuid NOT NULL REFERENCES account (id),
  xuid         bigint NOT NULL,
  tier         text NOT NULL CHECK (tier IN ('free', 'vip')),
  state        text NOT NULL CHECK (state IN ('issued', 'used', 'released', 'expired', 'kicked')),
  issued_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at   timestamptz NOT NULL,
  used_at      timestamptz,
  ended_at     timestamptz,
  -- join.granted written (the first move out of `issued`).
  announced    boolean NOT NULL DEFAULT false,
  CONSTRAINT mm_grant_ended_shape CHECK ((state IN ('issued', 'used')) = (ended_at IS NULL)),
  CONSTRAINT mm_grant_ttl CHECK (expires_at > issued_at AND expires_at <= issued_at + interval '60 seconds')
);

CREATE UNIQUE INDEX mm_grant_one_live ON mm_grant (game, profile_id) WHERE state IN ('issued', 'used');
CREATE INDEX mm_grant_world_live ON mm_grant (world_id) WHERE state IN ('issued', 'used');
CREATE INDEX mm_grant_free_live ON mm_grant (game, expires_at) WHERE tier = 'free' AND state IN ('issued', 'used');
CREATE INDEX mm_grant_issued_expiry ON mm_grant (expires_at) WHERE state = 'issued';
CREATE INDEX mm_grant_world_kicked ON mm_grant (world_id, ended_at) WHERE state = 'kicked';

-- The free queue: free joiners beyond the free limit, in arrival order; an entry not re-asked
-- within 30 s lapses.
CREATE TABLE mm_queue (
  id         uuid PRIMARY KEY,
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  world_id   uuid NOT NULL REFERENCES world (id),
  queued_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at timestamptz NOT NULL,
  CONSTRAINT mm_queue_one_per_profile UNIQUE (game, profile_id)
);

CREATE INDEX mm_queue_order ON mm_queue (game, queued_at, id);

-- The free limit of each game (configuration; owner default 50, D-new). Free joins of a game
-- serialize on the advisory lock fi_db::matchmaking takes on this row's game, then count the live
-- free grants (mm_grant_free_live); Supporter grants are never counted.
CREATE TABLE mm_free_slot (
  game        text PRIMARY KEY REFERENCES game (code),
  free_limit  integer NOT NULL DEFAULT 50 CHECK (free_limit >= 0),
  queue_open  boolean NOT NULL DEFAULT true,
  queue_limit integer NOT NULL DEFAULT 500 CHECK (queue_limit >= 0),
  updated_at  timestamptz NOT NULL DEFAULT clock_timestamp()
);

INSERT INTO mm_free_slot (game) VALUES ('fh2');

-- 7. The public aggregates of the 3-day gold build (fi-cli gold-build), which core serves with a
-- 10-minute cache; one row per (game, name), replaced by each build.
CREATE TABLE gold_snapshot (
  game     text NOT NULL REFERENCES game (code),
  name     text NOT NULL CHECK (name ~ '^[a-z][a-z0-9_]{0,63}$'),
  built_at timestamptz NOT NULL,
  payload  jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
  PRIMARY KEY (game, name)
);

GRANT SELECT, INSERT, UPDATE ON board_entry, player_position, ghost TO fi_runtime;
GRANT SELECT, INSERT ON ghost_blob TO fi_runtime;
GRANT SELECT, INSERT, UPDATE ON universe_refusal TO fi_runtime;
GRANT SELECT, INSERT, UPDATE ON world, mm_grant TO fi_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON world_ban, mm_queue TO fi_runtime;
GRANT SELECT ON mm_free_slot, gold_snapshot TO fi_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON gold_snapshot, mm_free_slot TO fi_maintenance;
GRANT SELECT, DELETE ON board_entry, player_position, ghost, ghost_blob, universe_refusal, world_ban, mm_queue TO fi_maintenance;
GRANT SELECT, UPDATE ON world, mm_grant TO fi_maintenance;
