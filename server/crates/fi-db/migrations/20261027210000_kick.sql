-- Moderators kick a player (owner-decisions 2026-10-27 "Moderators kick a player; the launcher gets
-- quick moderation actions"; CORE_API.md §5 `POST /core/v1/admin/kicks`, GATE_API.md §2, register
-- CR-S62). A kick ends the account's live play sessions (`state = 'revoked'`, the new end reason
-- `kicked`) without a ban, and may refuse new admissions for a short cooldown.
--
-- Expand only: the new columns are NULL for every account never kicked, and the previous build
-- neither reads them nor writes `kicked`.

-- `kicked` joins the end reasons (a moderator's kick; the session's state is `revoked`).
-- NOT VALID, then VALIDATE: the ACCESS EXCLUSIVE lock of the swap is held without the scan (the
-- validation scans under SHARE UPDATE EXCLUSIVE, which admissions and heartbeats do not wait for).
ALTER TABLE play_session DROP CONSTRAINT play_session_end_reason_check;
ALTER TABLE play_session ADD CONSTRAINT play_session_end_reason_check CHECK (end_reason IN
  ('game_exit', 'player', 'takeover', 'expired', 'revoked', 'queue_left', 'max_age', 'idle', 'kicked')) NOT VALID;
ALTER TABLE play_session VALIDATE CONSTRAINT play_session_end_reason_check;

-- `kicked_at`: the latest kick (the player's `kicked` notice; a session created before it is never
-- resumed by a heartbeat). `kicked_until`: admission is refused until then (`SESSIONS.KICKED`).
ALTER TABLE account
  ADD COLUMN kicked_at    timestamptz,
  ADD COLUMN kicked_until timestamptz,
  ADD CONSTRAINT account_kick_shape CHECK (kicked_until IS NULL OR kicked_at IS NOT NULL) NOT VALID;
ALTER TABLE account VALIDATE CONSTRAINT account_kick_shape;
