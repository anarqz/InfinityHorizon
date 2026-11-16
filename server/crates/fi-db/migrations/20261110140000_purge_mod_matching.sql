-- T-1110-NOMODMATCH (OD-235): mods never decide who can join or which world a player lands in.
-- The server keeps each world's, seat's and request's declared set (`mods`) for display only and
-- drops what existed only to match sets: the set keys and the per-side list indexes. Forward
-- migration: the applied ones stay immutable. `world_open_list` (20261019170000) serves the list.
DROP INDEX world_open_list_vanilla;
DROP INDEX world_open_list_modded;

-- The official shape (20261108160000) without the key: an official world declares no mods.
ALTER TABLE world DROP CONSTRAINT world_host_kind_shape;
ALTER TABLE world DROP CONSTRAINT world_mods_shape;
ALTER TABLE world DROP COLUMN mods_key;
ALTER TABLE world ADD CONSTRAINT world_mods_shape
  CHECK (jsonb_typeof(mods) = 'array' AND jsonb_array_length(mods) <= 100);
ALTER TABLE world
  ADD CONSTRAINT world_host_kind_shape CHECK (
    (host_kind = 'player' AND host_profile_id IS NOT NULL AND host_account_id IS NOT NULL AND host_vm_id IS NULL
        AND NOT official_group)
    OR (host_kind = 'official' AND host_profile_id IS NULL AND host_account_id IS NULL AND host_session_id IS NULL
        AND host_vm_id ~ '^[a-z0-9-]{1,32}$' AND NOT dedicated AND transport = 'direct' AND relay_key IS NULL
        AND audience = 'everyone' AND kind = 'free' AND host_tier = 'free' AND jsonb_array_length(mods) = 0
        AND allow IS NULL
        AND (activity <> 'carmeet' OR (any_mode AND NOT official_group)))
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_host_kind_shape;

ALTER TABLE mm_grant DROP CONSTRAINT mm_grant_mods_key_shape;
ALTER TABLE mm_grant DROP COLUMN mods_key;
