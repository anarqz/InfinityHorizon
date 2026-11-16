-- CR-S67 (MATCHMAKING_API.md §2.5; OD-154, OD-163): the lobby's voice setting, set by its host's
-- launcher and changed at any time while the world is open. `proximity` is the clients' (each
-- talker placed where their car is, with Doppler); the server only carries it.
ALTER TABLE world ADD COLUMN voice text NOT NULL DEFAULT 'open'
  CONSTRAINT world_voice_known CHECK (voice IN ('off', 'open', 'proximity'));
