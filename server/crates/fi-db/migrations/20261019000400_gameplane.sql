-- Platform v1, SER-M3 steps 3-4 (docs/contracts/GAME_PLATFORM.md §3a; docs/server/SER-M3_PORT_MAP.md
-- §1.2, §2): what the game plane keeps from the game's own proxied requests on the Supporter path.
-- The Beta's SQLite tables `users` (logon state), `profile`, `profile_tables`, `transactions` and
-- `presence` (deprecated/beta/server/src/db.rs) become these five, keyed by profile instead of XUID.
--
-- Every table carries `game` as the first column of its keys (ADR 0902 §1). Writers are the proxied
-- handlers of fi-api (`fi_api::game::proxy`) through fi_db::gameplane, each in the request's one
-- transaction; nothing here is written by a job. The runtime role never deletes: replies kept for
-- Transaction.Reconcile and stale presence are pruned by fi_maintenance (retention job, not built).

-- 1. The logon state of a profile: what User.Logon and User.SetPlayerCardInfo report that the
-- server serves back (Xls User.GetUserData, the Logon reply's userData), and the checkpoint of the
-- last OnlineProfile.UpdateAllTables. Only these fields of the game's UserLogonData are kept: its
-- IP address, MAC address, machine id, friend list and car ids are never stored.
CREATE TABLE game_logon (
  game            text NOT NULL REFERENCES game (code),
  profile_id      uuid NOT NULL REFERENCES profile (id),
  career_level    bigint NOT NULL DEFAULT 0 CHECK (career_level BETWEEN 0 AND 4294967295),
  badge_id        integer NOT NULL DEFAULT 0 CHECK (badge_id BETWEEN 0 AND 65535),
  player_title_id integer NOT NULL DEFAULT 0 CHECK (player_title_id BETWEEN 0 AND 65535),
  title_version   integer,
  -- transactionId of the last UpdateAllTables stored (the Logon reply's
  -- lastOnlineProfileUpdateAllTablesTranId); NULL until the first one.
  tables_tran_id  integer,
  last_session_id uuid REFERENCES play_session (id),
  first_logon_at  timestamptz,
  last_logon_at   timestamptz,
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id)
);

-- 2. The online profile (OnlineProfile.UpdateAll): the last upload with each section the game
-- marks unchanged (an empty list) kept from before. Settings, assists and career statistics; the
-- numbers that matter to the lake also go out as `profile_sync` progression facts.
CREATE TABLE game_profile (
  game        text NOT NULL REFERENCES game (code),
  profile_id  uuid NOT NULL REFERENCES profile (id),
  data        jsonb NOT NULL CHECK (jsonb_typeof(data) = 'object'),
  updated_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version bigint NOT NULL DEFAULT 1,
  PRIMARY KEY (game, profile_id),
  -- At most 1 MiB stored (the game's sections are a few KiB).
  CONSTRAINT game_profile_size CHECK (pg_column_size(data) <= 1048576)
);

-- 3. The profile's tables (OnlineProfile.UpdateAllTables: garage cars, purchased parts, track
-- stats, ...): one row per table, `data` an object of the table's rows by their key (`id`, `Id`
-- or `trackId`), merged upload after upload; `delete<Table>` lists remove keys.
CREATE TABLE game_profile_table (
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  name       text NOT NULL CHECK (name ~ '^[A-Za-z][A-Za-z0-9_]{0,63}$'),
  data       jsonb NOT NULL CHECK (jsonb_typeof(data) = 'object'),
  updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id, name),
  -- At most 4 MiB stored (a full garage is a few hundred KiB).
  CONSTRAINT game_profile_table_size CHECK (pg_column_size(data) <= 4194304)
);

-- 4. Replies of transactional requests (X-TransactionId), byte for byte with their 12-byte xtsw
-- header, for Xls Transaction.Reconcile, which hands them back to a game that missed them. The
-- latest reply of a transaction id wins (the Beta's INSERT OR REPLACE). A reply larger than the
-- game's blob limit (0x80000) cannot be reconciled and is not kept.
CREATE TABLE game_transaction (
  game           text NOT NULL REFERENCES game (code),
  profile_id     uuid NOT NULL REFERENCES profile (id),
  transaction_id uuid NOT NULL,
  -- `Class.Method` of the request.
  method         text NOT NULL CHECK (char_length(method) BETWEEN 1 AND 128),
  reply          bytea NOT NULL CHECK (octet_length(reply) BETWEEN 12 AND 524288),
  session_id     uuid REFERENCES play_session (id),
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id, transaction_id)
);

CREATE INDEX game_transaction_age ON game_transaction (created_at);

-- 5. The Xbox Live presence a profile's game posts (userpresence.xboxlive.com): state and rich
-- presence id (the game's own RP_* names). game_presence (one row per session and instance) is
-- the platform's evidence of which pool served a session; this is what friends see.
CREATE TABLE game_player_presence (
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  state      text NOT NULL CHECK (state IN ('active', 'inactive', 'offline')),
  rich       text NOT NULL CHECK (rich ~ '^[A-Za-z0-9_]{0,40}$'),
  session_id uuid REFERENCES play_session (id),
  updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id)
);

GRANT SELECT, INSERT, UPDATE ON game_logon, game_profile, game_profile_table, game_transaction, game_player_presence TO fi_runtime;
GRANT SELECT, DELETE ON game_logon, game_profile, game_profile_table, game_transaction, game_player_presence TO fi_maintenance;
