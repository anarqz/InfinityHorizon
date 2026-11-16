-- SER-M3 step 6 (OD-48; GAME_PLATFORM.md §3a "Supporter universe", BUG-0001): the players'
-- drivatars, ported from the Beta's `drivatars` table (deprecated/beta/server/src/db.rs), keyed by
-- game, mode and profile instead of the XUID.
--
-- One row per (game, mode, profile): the latest Drivatar.SaveDrivatarInformation of the profile in
-- that mode (the game uploads its whole model each time, so the latest replaces the previous),
-- bounded to what the game's DrivatarRacingInfo can carry back (20 curves, 200 segments). `trained`
-- is decided by the writer (fi_api::game::proxy::universe::drivatars): an upload whose behaviour
-- model has no observation time yet is stored (its owner's game keeps training it from there) but
-- never served to others. Written only by the proxied handler, in the request's transaction with
-- its `drivatar` progression fact (`event_id`), through fi_db::drivatars. A profile that moves to
-- the other mode leaves this mode's row behind, unseen (fi_db::visibility).
CREATE TABLE game_drivatar (
  game       text NOT NULL REFERENCES game (code),
  mode       text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  profile_id uuid NOT NULL REFERENCES profile (id),
  -- DrivatarInfo: the 29 numbers of the behaviour model.
  info       jsonb NOT NULL CHECK (jsonb_typeof(info) = 'object'),
  -- CurveInfo list, at most 20 (DrivatarRacingInfo.Curves).
  curves     jsonb NOT NULL CHECK (jsonb_typeof(curves) = 'array' AND jsonb_array_length(curves) <= 20),
  -- SegmentInfo list, at most 200 (DrivatarRacingInfo.Segments).
  segments   jsonb NOT NULL CHECK (jsonb_typeof(segments) = 'array' AND jsonb_array_length(segments) <= 200),
  trained    boolean NOT NULL,
  -- Uploads stored (a replayed request id is not one).
  saves      bigint NOT NULL DEFAULT 1 CHECK (saves >= 1),
  -- When the latest upload was stored.
  trained_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- The progression fact of the latest upload (UUIDv5 of the request, GAME_PLATFORM.md §3a).
  event_id   uuid NOT NULL,
  session_id uuid REFERENCES play_session (id),
  PRIMARY KEY (game, mode, profile_id),
  -- 200 segments of 11 numbers stay under 100 KiB; 256 KiB is the ceiling.
  CONSTRAINT game_drivatar_size CHECK (pg_column_size(info) + pg_column_size(curves) + pg_column_size(segments) <= 262144)
);

-- The candidates of a race or of the cache: the trained drivatars of a game and mode, most
-- recently trained first.
CREATE INDEX game_drivatar_recent ON game_drivatar (game, mode, trained_at DESC, profile_id) WHERE trained;

GRANT SELECT, INSERT, UPDATE ON game_drivatar TO fi_runtime;
GRANT SELECT, DELETE ON game_drivatar TO fi_maintenance;
