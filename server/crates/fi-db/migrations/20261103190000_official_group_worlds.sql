-- OD-191 (WORLDS_OFFICIAL.md §11.4, MATCHMAKING_API.md §2.9): a game-mode session started in an
-- official car meet opens an official group world for the accepted players only: never quick-joined
-- or listed, closed once no seat in it is live. A mixed group admits both modes (`any_mode`, OD-141)
-- whatever its activity. Forward only: applied migrations are immutable.

ALTER TABLE world ADD COLUMN official_group boolean NOT NULL DEFAULT false;

ALTER TABLE world DROP CONSTRAINT world_host_kind_shape;
ALTER TABLE world
  ADD CONSTRAINT world_host_kind_shape CHECK (
    (host_kind = 'player' AND host_profile_id IS NOT NULL AND host_account_id IS NOT NULL AND host_vm_id IS NULL
        AND NOT official_group)
    OR (host_kind = 'official' AND host_profile_id IS NULL AND host_account_id IS NULL AND host_session_id IS NULL
        AND host_vm_id ~ '^[a-z0-9-]{1,32}$' AND NOT dedicated AND transport = 'direct' AND relay_key IS NULL
        AND NOT pool AND audience = 'everyone' AND kind = 'free' AND host_tier = 'free' AND mods_key IS NULL
        AND allow IS NULL
        AND (activity <> 'carmeet' OR (any_mode AND NOT official_group))
        AND (official_group OR any_mode = (activity = 'carmeet')))
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_host_kind_shape;
