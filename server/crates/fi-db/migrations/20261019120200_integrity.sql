-- SER-M3 step 6 (OD-48; docs/contracts/GAME_PLATFORM.md §3a "Supporter universe", RPC gates): what
-- the game plane last accepted of each profile's sync, so the next one is measured against it
-- (fi_gamedb::gates, written through fi_db::integrity in the request's one transaction). The
-- Beta's SQLite `integrity` table (deprecated/beta/server/src/db/integrity.rs) kept the baseline
-- and the strikes in one row per XUID; its strikes are `integrity_strike` here (migration
-- 20261019115900_rpc_strikes.sql), and its baseline becomes this table.
--
-- `game` first (ADR 0902 §1). One row per profile and mode: a profile whose mode changed starts a
-- new baseline (its first sync in the new mode seeds it, as a new profile's does). The two halves
-- fill independently: the profile sync's numbers (OnlineProfile.UpdateAll) and the garage's size
-- (OnlineProfile.UpdateAllTables), each NULL until the first accepted sync of its kind seeds it.
-- Times are the database clock, the one the gates measure with.
CREATE TABLE integrity_baseline (
  game             text NOT NULL REFERENCES game (code),
  profile_id       uuid NOT NULL REFERENCES profile (id),
  mode             text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  -- The last accepted UpdateAll (u32 in the game; a Noob profile has no upper limit).
  credits          bigint CHECK (credits >= 0),
  xp               bigint CHECK (xp >= 0),
  level            bigint CHECK (level >= 0),
  winnings         bigint CHECK (winnings >= 0),
  -- m/s, the top speed record of that sync: a later record is checked only when it is higher.
  top_speed_mps    double precision CHECK (top_speed_mps >= 0 AND top_speed_mps < 'Infinity'::double precision),
  synced_at        timestamptz,
  -- The garage's size after the last accepted UpdateAllTables.
  garage_cars      integer CHECK (garage_cars >= 0),
  tables_synced_at timestamptz,
  updated_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id, mode),
  CONSTRAINT integrity_baseline_profile_shape
    CHECK (num_nulls(credits, xp, level, winnings, top_speed_mps, synced_at) IN (0, 6)),
  CONSTRAINT integrity_baseline_tables_shape
    CHECK (num_nulls(garage_cars, tables_synced_at) IN (0, 2))
);

GRANT SELECT, INSERT, UPDATE ON integrity_baseline TO fi_runtime;
-- Erasure and retention (not built) delete a profile's baseline with its game data.
GRANT SELECT, DELETE ON integrity_baseline TO fi_maintenance;
