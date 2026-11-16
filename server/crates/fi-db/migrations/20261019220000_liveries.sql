-- OD-65 (2026-10-19; GAME_PLATFORM.md §3a "Liveries", register CR-S25): a player's own paint
-- reaches the players sharing its world on every tier. Ported from the Beta's `liveries` table
-- (deprecated/beta/server/src/db.rs add_livery / livery), keyed by game and profile. Written only
-- by the proxied handler (fi_api::game::proxy::liveries) through fi_db::liveries.
--
-- Retention: a row with `session_id` is TEMPORARY (a request that keeps no cloud data: free tier
-- or noob mode, OD-52): it belongs to the play session that wrote it and is deleted when that
-- session leaves `queued`/`active` (gate leave, sweep expiry, revocation by a ban: the trigger
-- below, whatever path ends it) or when the session row goes (ON DELETE CASCADE). Reads also
-- require the session to be live, so the end is immediate. A row without `session_id` is a
-- Supporter Hardcore's PERMANENT cloud data: a cloud_purge_target, so a Hardcore -> Noob change
-- deletes it.

-- 1. The pending copy: OnlineProfile.UpdateLivery, the design's bytes by (name, car). The game
--    names the design with CreateVersionedLivery right after, which copies these bytes.
CREATE TABLE game_livery_draft (
  game             text NOT NULL REFERENCES game (code),
  profile_id       uuid NOT NULL REFERENCES profile (id),
  name             text NOT NULL CHECK (char_length(name) <= 64),
  car_id           integer NOT NULL,
  number_of_layers smallint NOT NULL,
  -- VersionedLiveryData.data's max_len in the game's schema: 512 KiB.
  data             bytea NOT NULL CHECK (octet_length(data) BETWEEN 1 AND 524288),
  session_id       uuid REFERENCES play_session (id) ON DELETE CASCADE,
  updated_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id, name, car_id)
);

CREATE INDEX game_livery_draft_session ON game_livery_draft (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX game_livery_draft_recent ON game_livery_draft (game, profile_id, updated_at DESC);

-- 2. The versioned liveries peers fetch by id (RetrieveVersionedLiveryById). `id` is derived from
--    the request's fact id (UUIDv5 of the session and the request), so a replay answers the same
--    id; never sequential. `data` is NULL when no pending copy of the name existed (the Beta's
--    data-less row): never served.
CREATE TABLE game_livery (
  id               bigint PRIMARY KEY CHECK (id > 0),
  game             text NOT NULL REFERENCES game (code),
  mode             text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  profile_id       uuid NOT NULL REFERENCES profile (id),
  name             text NOT NULL CHECK (char_length(name) <= 64),
  car_id           integer NOT NULL,
  number_of_layers smallint NOT NULL,
  data             bytea CHECK (data IS NULL OR octet_length(data) BETWEEN 1 AND 524288),
  session_id       uuid REFERENCES play_session (id) ON DELETE CASCADE,
  created_at       timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX game_livery_session ON game_livery (session_id, created_at) WHERE session_id IS NOT NULL;
CREATE INDEX game_livery_profile ON game_livery (game, profile_id, created_at DESC);

-- 3. A session that ends takes its temporary liveries with it. SECURITY DEFINER: every role that
--    ends a session (runtime, maintenance) does so without DELETE on these tables.
CREATE FUNCTION fi_livery_session_ended() RETURNS trigger
LANGUAGE plpgsql VOLATILE SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
BEGIN
  DELETE FROM public.game_livery WHERE session_id = NEW.id;
  DELETE FROM public.game_livery_draft WHERE session_id = NEW.id;
  RETURN NULL;
END
$$;
REVOKE ALL ON FUNCTION fi_livery_session_ended() FROM PUBLIC;

CREATE TRIGGER play_session_livery_end AFTER UPDATE OF state ON play_session
  FOR EACH ROW WHEN (OLD.state IN ('queued', 'active') AND NEW.state NOT IN ('queued', 'active'))
  EXECUTE FUNCTION fi_livery_session_ended();

-- 4. Supporter Hardcore liveries are cloud data (OD-52). Temporary rows of the profile are gone
--    by then (a mode change needs no live session).
INSERT INTO cloud_purge_target (table_name, column_name, category) VALUES
  ('game_livery', 'profile_id', 'liveries'),
  ('game_livery_draft', 'profile_id', NULL);

-- The runtime writes and prunes its own rows (RemoveReferenceToVersionedLivery, the caps).
GRANT SELECT, INSERT, DELETE ON game_livery, game_livery_draft TO fi_runtime;
GRANT UPDATE (number_of_layers, data, session_id, updated_at) ON game_livery_draft TO fi_runtime;
GRANT SELECT, DELETE ON game_livery, game_livery_draft TO fi_maintenance;
