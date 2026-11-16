-- Local environment v0, workstream A (docs/server/LOCAL_ENV.md §4.1; docs/contracts/GAME_PLATFORM.md
-- §2.3): the game plane's presence record, and the launch nonce of a play session.
--
-- One row per play session that reached a game-plane instance: the pool and the instance that last
-- answered it (FI_POOL, FI_INSTANCE_ID), first and last seen by the database clock. It is the
-- server-side evidence that a session was only ever served by its own pool's deployment
-- (invariant I4). Written by `POST /game/v1/presence` (fi_db::game::presence_in), upserted, so a
-- retried or repeated call changes only last_seen_at and the instance. Rows of ended sessions stay
-- for the demo; production deletes them with the session's retention (fi_maintenance).

CREATE TABLE game_presence (
  session_id    uuid PRIMARY KEY REFERENCES play_session (id),
  profile_id    uuid NOT NULL REFERENCES profile (id),
  pool          text NOT NULL REFERENCES pool (name),
  instance      text NOT NULL CHECK (char_length(instance) BETWEEN 1 AND 128),
  first_seen_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  last_seen_at  timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX game_presence_pool ON game_presence (pool, last_seen_at);

GRANT SELECT, INSERT, UPDATE ON game_presence TO fi_runtime;
GRANT SELECT, DELETE ON game_presence TO fi_maintenance;

-- The launcher's handoff nonce (SAVES_API.md §3.1 `nonce`, 32 hex digits) is echoed into the launch
-- ticket. A queued session is admitted on a later poll, which carries no body, so the nonce of the
-- create is kept with the session to sign that ticket. Not a secret: the ticket itself carries it.
ALTER TABLE play_session ADD COLUMN launch_nonce text CHECK (launch_nonce IS NULL OR launch_nonce ~ '^[0-9a-f]{32}$');
