-- Platform v1, the gate (ADR 0015, ADR 0902; docs/contracts/GATE_API.md): pools and play sessions
-- per game, the liveness signal of game requests, the current ticket of every session, the
-- game's maintenance switch, and the matchmaking pool of free admissions.
--
-- Pool names stay globally unique (the primary key other tables reference); `game` says which
-- title a pool serves and play_session (pool, game) must match it. A second title's pools get
-- their own names until a migration moves the key to (game, name) (OPEN_QUESTIONS, gate lane).

-- The game's maintenance switch (core writes it; the gate refuses new admissions while it is in
-- the future, SERVICE.MAINTENANCE). IF NOT EXISTS: core's migration may add the same column.
ALTER TABLE game ADD COLUMN IF NOT EXISTS maintenance_until timestamptz;

-- Pools per game. The default keeps the local environment v0's pool rows and tests working; the
-- PV1-I2 re-cut drops it once every writer names the game.
ALTER TABLE pool ADD COLUMN game text NOT NULL DEFAULT 'fh2' REFERENCES game (code);
ALTER TABLE pool ADD CONSTRAINT pool_name_game_key UNIQUE (name, game);
CREATE INDEX pool_by_game ON pool (game, tier);

-- Free admissions go to the game's matchmaking pool (tier free, no save lease, server = the
-- matchmaking origin; CR-L16). Capacity = the free concurrency limit (O-11 default 50). The
-- endpoint is the local ingress; FI_POOLS_JSON (fi-cli pools-sync) sets the real origin.
INSERT INTO pool (name, game, tier, capacity, queue_limit, game_endpoint)
SELECT 'matchmaking', code, 'free', 50, 20000, 'http://127.0.0.1:8480'
FROM game WHERE code = 'fh2'
ON CONFLICT (name) DO NOTHING;

-- Play sessions per game: the column equals the profile's game (trigger below, since a CHECK
-- cannot read another table) and the pool's (composite foreign key).
ALTER TABLE play_session ADD COLUMN game text REFERENCES game (code);
UPDATE play_session s SET game = p.game FROM profile p WHERE p.id = s.profile_id;
ALTER TABLE play_session ALTER COLUMN game SET NOT NULL;
ALTER TABLE play_session ADD CONSTRAINT play_session_pool_game_fkey
  FOREIGN KEY (pool, game) REFERENCES pool (name, game);

CREATE FUNCTION fi_play_session_same_game() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  profile_game text;
BEGIN
  SELECT p.game INTO profile_game FROM profile p WHERE p.id = NEW.profile_id;
  IF NEW.game IS NULL THEN
    NEW.game := profile_game;
  ELSIF NEW.game IS DISTINCT FROM profile_game THEN
    RAISE EXCEPTION 'play_session.game must equal its profile''s game'
      USING ERRCODE = 'check_violation', CONSTRAINT = 'play_session_same_game';
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER play_session_same_game BEFORE INSERT OR UPDATE OF game, profile_id ON play_session
  FOR EACH ROW EXECUTE FUNCTION fi_play_session_same_game();

-- Liveness and the current ticket (ADR 0015 §1, §3):
-- - last_seen_at: the last authenticated game request (saves and matchmaking, batched at most
--   once per 60 s per instance: fi_db::gate::touch_seen). Never renews the save lease; it keeps
--   the slot while the lease passes on both signals.
-- - lease_ttl_ms: the slot's lease length at the last admission or heartbeat (FI_LEASE_TTL_MS),
--   so the sweeper needs no configuration. At most 60 minutes: the owner's ceiling.
-- - save_line: whether the session holds the profile's save lease (vip; every session of the
--   local environment v0 and of the /launcher/v1 aliases). Free gate sessions do not (OD-24).
-- - ticket_kid, ticket_exp: the ticket last minted for the session (never the token itself).
ALTER TABLE play_session
  ADD COLUMN last_seen_at timestamptz,
  ADD COLUMN lease_ttl_ms integer CONSTRAINT play_session_lease_ceiling CHECK (lease_ttl_ms BETWEEN 1000 AND 3600000),
  ADD COLUMN save_line boolean NOT NULL DEFAULT true,
  ADD COLUMN ticket_kid text CHECK (char_length(ticket_kid) BETWEEN 1 AND 32),
  ADD COLUMN ticket_exp timestamptz,
  ADD CONSTRAINT play_session_ticket_shape CHECK ((ticket_kid IS NULL) = (ticket_exp IS NULL));

-- Indexes with game first (ADR 0902). The FIFO queue and the active count per pool replace the
-- v0 ones; the sweeper scans by expiry and by maximum age across games; queue ETAs read recent
-- admissions.
DROP INDEX play_session_queue;
DROP INDEX play_session_active_by_pool;
CREATE INDEX play_session_queue ON play_session (game, pool, queued_at, id) WHERE state = 'queued';
CREATE INDEX play_session_active_by_pool ON play_session (game, pool, expires_at) WHERE state = 'active';
CREATE INDEX play_session_admitted ON play_session (game, pool, admitted_at) WHERE admitted_at IS NOT NULL;
CREATE INDEX play_session_sweep_expiry ON play_session (expires_at) WHERE state IN ('queued', 'active');
CREATE INDEX play_session_sweep_max_age ON play_session (max_until) WHERE state = 'active';
