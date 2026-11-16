-- T-1110-MODS100 (owner-approved): the multiplayer mod set holds at most 100 entries (was 32,
-- MODS_MAX). Forward migration: the applied ones stay immutable. Widening only, so every
-- existing row satisfies the new checks. world_mods_shape keeps its other conditions.
ALTER TABLE world DROP CONSTRAINT world_mods_shape;
ALTER TABLE world ADD CONSTRAINT world_mods_shape CHECK (
  jsonb_typeof(mods) = 'array' AND jsonb_array_length(mods) <= 100
  AND (mods_key IS NULL) = (jsonb_array_length(mods) = 0)
  AND (mods_key IS NULL OR mods_key ~ '^[0-9a-f]{64}$')
);

ALTER TABLE mm_grant DROP CONSTRAINT mm_grant_mods_shape;
ALTER TABLE mm_grant ADD CONSTRAINT mm_grant_mods_shape
  CHECK (jsonb_typeof(mods) = 'array' AND jsonb_array_length(mods) <= 100);

ALTER TABLE world_invite DROP CONSTRAINT world_invite_mods_shape;
ALTER TABLE world_invite ADD CONSTRAINT world_invite_mods_shape
  CHECK (jsonb_typeof(mods) = 'array' AND jsonb_array_length(mods) <= 100);
