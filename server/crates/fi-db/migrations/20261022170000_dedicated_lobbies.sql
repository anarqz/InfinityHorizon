-- Dedicated lobbies (2026-10-22; OD-109; docs/contracts/GATE_API.md §2 "Dedicated sessions",
-- MATCHMAKING_API.md §2.6). An admin's launcher hosts a matchmaking world 24/7 with no game
-- process: the gate admits a dedicated play session (`POST /gate/v1/sessions {dedicated: true}`),
-- and the world it opens is dedicated.
--
-- A dedicated session holds no save lease, is never queued, is outside pool and online capacity
-- and every active-session count, and coexists with one ordinary session of the same account
-- (often of the same profile): the one-live-session-per-profile index leaves dedicated rows out,
-- and a second index allows at most one live dedicated session per account.
ALTER TABLE play_session ADD COLUMN dedicated boolean NOT NULL DEFAULT false;
ALTER TABLE play_session ADD CONSTRAINT play_session_dedicated_shape
  CHECK (NOT dedicated OR (queued_at IS NULL AND NOT save_line));

DROP INDEX play_session_one_live_per_profile;
CREATE UNIQUE INDEX play_session_one_live_per_profile ON play_session (profile_id)
  WHERE state IN ('queued', 'active') AND NOT dedicated;
CREATE UNIQUE INDEX play_session_one_live_dedicated_per_account ON play_session (account_id)
  WHERE state IN ('queued', 'active') AND dedicated;

-- A world opened with a dedicated session's ticket (copied at the world's insert; its host
-- session never changes): the host is not a player and the OD-107 idle close does not apply.
ALTER TABLE world ADD COLUMN dedicated boolean NOT NULL DEFAULT false;
