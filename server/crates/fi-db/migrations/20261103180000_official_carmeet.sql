-- OD-190 (WORLDS_OFFICIAL.md §11.1, MATCHMAKING_API.md §2.9): an official world runs one activity for
-- its life, `freeroam`, `carmeet` or `roadtrip`. `carmeet` is official-only (a player world's activity
-- stays `freeroam` | `roadtrip`, OD-150), and an official car meet admits both modes (OD-180:
-- `any_mode`), which the official shape refused until now. Forward only: applied migrations are
-- immutable.

ALTER TABLE world DROP CONSTRAINT world_activity_known;
ALTER TABLE world
  ADD CONSTRAINT world_activity_known CHECK (
    activity IN ('freeroam', 'roadtrip') OR (activity = 'carmeet' AND host_kind = 'official')
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_activity_known;

ALTER TABLE world DROP CONSTRAINT world_host_kind_shape;
ALTER TABLE world
  ADD CONSTRAINT world_host_kind_shape CHECK (
    (host_kind = 'player' AND host_profile_id IS NOT NULL AND host_account_id IS NOT NULL AND host_vm_id IS NULL)
    OR (host_kind = 'official' AND host_profile_id IS NULL AND host_account_id IS NULL AND host_session_id IS NULL
        AND host_vm_id ~ '^[a-z0-9-]{1,32}$' AND NOT dedicated AND transport = 'direct' AND relay_key IS NULL
        AND NOT pool AND audience = 'everyone' AND kind = 'free' AND host_tier = 'free' AND mods_key IS NULL
        AND allow IS NULL AND any_mode = (activity = 'carmeet'))
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_host_kind_shape;

-- The quick join's scan: the oldest open official world of a game and activity (a car meet of any
-- mode; the mode is checked on the few rows of the activity).
CREATE INDEX world_open_official_activity ON world (game, activity, opened_at, id)
  WHERE closed_at IS NULL AND host_kind = 'official';
