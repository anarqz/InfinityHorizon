-- The pineapple pool (OD-147, OD-149, OD-158; MATCHMAKING_API.md §3.9, register CR-S79; reference
-- parity S15b): a Hardcore profile under a live `cheat_flag` plays only in pineapple worlds, and
-- everybody sees which worlds those are (fi_db::observations, fi_db::matchmaking).
--
-- Expand only: every column has a default the previous build never reads; the new reason values
-- are written only by this build.

-- A world opened by a profile flagged at that moment (Hardcore, never dedicated) is a pineapple
-- world for its life. Written once, when the world opens.
ALTER TABLE world ADD COLUMN pool boolean NOT NULL DEFAULT false;

-- `pool`: on the flag, a clean world its host hosts closes (its seats end `world_closed`), and the
-- profile's live seat in a clean world ends `pool`. NOT VALID, then VALIDATE: the swap's ACCESS
-- EXCLUSIVE lock is held without the scan.
ALTER TABLE world DROP CONSTRAINT world_close_reason_check;
ALTER TABLE world ADD CONSTRAINT world_close_reason_check
  CHECK (close_reason IN ('host_closed', 'expired', 'replaced', 'session_ended', 'idle', 'pool')) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_close_reason_check;
ALTER TABLE mm_grant DROP CONSTRAINT mm_grant_end_reason_check;
ALTER TABLE mm_grant ADD CONSTRAINT mm_grant_end_reason_check
  CHECK (end_reason IN ('expired', 'released', 'kicked', 'world_closed', 'pool')) NOT VALID;
ALTER TABLE mm_grant VALIDATE CONSTRAINT mm_grant_end_reason_check;

-- The flag's repeat count: a flag lasts 2 hours, doubling with each new flag that starts within
-- 30 days of the previous one's end (2, 4, 8, 16, 32 h, at most 48 h). The row now outlives its
-- flag by those 30 days (retention), and a new flag updates it instead of replacing it.
ALTER TABLE cheat_flag ADD COLUMN episode integer NOT NULL DEFAULT 1
  CONSTRAINT cheat_flag_episode_positive CHECK (episode >= 1);
GRANT UPDATE (first_at, episode) ON cheat_flag TO fi_runtime;
