-- Idle worlds close (2026-10-22; OD-107; docs/contracts/MATCHMAKING_API.md §3.3 item 4,
-- EVENTS.md §3.8). The heartbeat or PUT of a world that has had nobody but its host for the idle
-- time (FI_MM_WORLD_IDLE_SECS, default 300 s) closes it with `close_reason = 'idle'`
-- (fi_db::matchmaking::host_with_idle).
--
-- The check reads a world's seats and offers of every state (the last one issued, sent or ended),
-- so both tables get an index by world that also covers the ended rows; the existing ones are
-- partial on the live states.
ALTER TABLE world DROP CONSTRAINT world_close_reason_check;
ALTER TABLE world ADD CONSTRAINT world_close_reason_check
  CHECK (close_reason IN ('host_closed', 'expired', 'replaced', 'session_ended', 'idle'));

CREATE INDEX mm_grant_world_activity ON mm_grant (world_id, ended_at);
CREATE INDEX world_invite_world_activity ON world_invite (world_id, ended_at);
