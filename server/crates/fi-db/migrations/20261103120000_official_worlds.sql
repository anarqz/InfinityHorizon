-- Official worlds (OD-188, OD-189; MATCHMAKING_API.md §2.9, WORLDS_OFFICIAL.md): a world hosted by a
-- Worlds host of FI_WORLDS (host_kind 'official', host_vm_id its vmId) has no host profile, account or
-- session. A player's world keeps exactly today's shape (host_kind 'player', the three host ids set,
-- no vmId). Forward only: applied migrations are immutable.

ALTER TABLE world
  ADD COLUMN host_kind text NOT NULL DEFAULT 'player',
  ADD COLUMN host_vm_id text;

ALTER TABLE world
  ALTER COLUMN host_profile_id DROP NOT NULL,
  ALTER COLUMN host_account_id DROP NOT NULL;

ALTER TABLE world
  ADD CONSTRAINT world_host_kind_shape CHECK (
    (host_kind = 'player' AND host_profile_id IS NOT NULL AND host_account_id IS NOT NULL AND host_vm_id IS NULL)
    OR (host_kind = 'official' AND host_profile_id IS NULL AND host_account_id IS NULL AND host_session_id IS NULL
        AND host_vm_id ~ '^[a-z0-9-]{1,32}$' AND NOT dedicated AND transport = 'direct' AND relay_key IS NULL
        AND NOT pool AND audience = 'everyone' AND kind = 'free' AND host_tier = 'free' AND mods_key IS NULL
        AND allow IS NULL AND NOT any_mode)
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_host_kind_shape;

-- An open player world still needs its host session; an official world has none.
ALTER TABLE world DROP CONSTRAINT world_open_has_host_session;
ALTER TABLE world
  ADD CONSTRAINT world_open_has_host_session CHECK (
    closed_at IS NOT NULL OR host_session_id IS NOT NULL OR host_kind = 'official'
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_open_has_host_session;

-- The placement scan (oldest open official world of a game, mode and activity) and the per-VM count.
CREATE INDEX world_open_official ON world (game, mode, activity, opened_at, id)
  WHERE closed_at IS NULL AND host_kind = 'official';
CREATE INDEX world_open_official_vm ON world (host_vm_id)
  WHERE closed_at IS NULL AND host_kind = 'official';
