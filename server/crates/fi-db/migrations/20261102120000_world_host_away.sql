-- S29 (OD-180.2 and its follow-up, MATCHMAKING_API.md §2.5/§3.3, register CR-S85): the host's game is
-- not in its world (it left free roam, exited or crashed) while the host's launcher keeps the world
-- running for the other players. Set by every heartbeat (`hostAway`, absent = false), echoed in
-- `WorldView.hostAway`. A heartbeat with `hostAway` and no other player left closes the world
-- (`close_reason = 'host_away'`). NOT VALID, then VALIDATE: the swap's ACCESS EXCLUSIVE lock is held
-- without the scan.
ALTER TABLE world ADD COLUMN host_away boolean NOT NULL DEFAULT false;

ALTER TABLE world DROP CONSTRAINT world_close_reason_check;
ALTER TABLE world ADD CONSTRAINT world_close_reason_check
  CHECK (close_reason IN ('host_closed', 'expired', 'replaced', 'session_ended', 'idle', 'pool', 'host_away')) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_close_reason_check;
