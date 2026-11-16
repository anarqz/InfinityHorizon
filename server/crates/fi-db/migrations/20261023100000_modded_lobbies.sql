-- Modded lobbies (2026-10-23; OD-112; docs/contracts/MATCHMAKING_API.md §2.7,
-- docs/launcher/MODDED_LOBBIES.md §2-§3). A world carries its host's mod set; a guest enters only
-- with exactly the same set (equal `mods_key`).
--
-- `mods` is the validated, sorted set as JSON (`[{id, version, fingerprint}]`, at most 32;
-- `[]` = vanilla); `mods_key` the lowercase hex SHA-256 of its canonical JSON (fi_db::matchmaking::
-- mods_key), NULL for a vanilla world. Both are written when the world opens and never change
-- (a PUT with another set is 409 PORTAL.CONFLICT {reason: "mods"}). Existing rows are vanilla.
ALTER TABLE world ADD COLUMN mods jsonb NOT NULL DEFAULT '[]';
ALTER TABLE world ADD COLUMN mods_key text;
ALTER TABLE world ADD CONSTRAINT world_mods_shape CHECK (
  jsonb_typeof(mods) = 'array' AND jsonb_array_length(mods) <= 32
  AND (mods_key IS NULL) = (jsonb_array_length(mods) = 0)
  AND (mods_key IS NULL OR mods_key ~ '^[0-9a-f]{64}$')
);

-- The key a seat was granted with (NULL = vanilla): free roam routes a seat only into a world of
-- the same key (defence in depth: a world's set is fixed, so they never differ today).
ALTER TABLE mm_grant ADD COLUMN mods_key text;
ALTER TABLE mm_grant ADD CONSTRAINT mm_grant_mods_key_shape
  CHECK (mods_key IS NULL OR mods_key ~ '^[0-9a-f]{64}$');

-- The list (`GET /mm/v1/worlds?modded=`) reads one side at a time, newest first: one partial index
-- per side, so the bounded scan (LIST_SCAN) never walks the other side's worlds.
CREATE INDEX world_open_list_vanilla ON world (game, mode, opened_at DESC, id)
  WHERE closed_at IS NULL AND mods_key IS NULL;
CREATE INDEX world_open_list_modded ON world (game, mode, opened_at DESC, id)
  WHERE closed_at IS NULL AND mods_key IS NOT NULL;
