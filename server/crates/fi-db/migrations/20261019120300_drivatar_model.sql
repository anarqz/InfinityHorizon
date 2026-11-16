-- The drivatars, part 2 (SER-M3 step 6; review of the drivatar lane, GAME_PLATFORM.md §3a
-- "Supporter universe"): the model is stored as the game's own encoding, not as jsonb.
--
-- 20261019120100 kept `info`, `curves` and `segments` as jsonb. Serving them costs their text
-- form on the wire: about 53 KB per full drivatar (200 segments, 20 curves) where the game's xtsw
-- encoding is under 10 KB, so a cache of 200 full drivatars shipped 10 MB out of Postgres for a
-- reply the game can take 1 MiB of. Now `model` holds the checked model encoded as the
-- SaveDrivatarInformation parameters it came in (fi_api::game::proxy::universe::drivatars):
-- compact, round-tripped by the same codec that answers the game, and its size is known without
-- reading it (`octet_length` of a stored bytea reads the TOAST header only), so a candidates
-- query stops at the reply's byte budget before it reads a single model or garage too many.
--
-- Written as a new migration, not an edit of 20261019120100, because that one is already applied
-- on the local stacks (sqlx refuses an edited applied migration). The rows are dropped, not
-- converted (SQL cannot run the game's codec): nothing here was imported (OD-48), every row is a
-- local test's or a local play session's, and a game uploads its whole model again after its next
-- race; until then its owner's race gets the minimal playerData and others do not see it.

DELETE FROM game_drivatar;

ALTER TABLE game_drivatar
  DROP CONSTRAINT game_drivatar_size,
  DROP COLUMN info,
  DROP COLUMN curves,
  DROP COLUMN segments,
  -- A full model (20 curves, 200 segments) encodes to 9,724 bytes.
  ADD COLUMN model bytea NOT NULL CONSTRAINT game_drivatar_model_size CHECK (octet_length(model) BETWEEN 1 AND 16384);

COMMENT ON COLUMN game_drivatar.model IS
  'The checked drivatar model, xtsw-encoded as Drivatar.SaveDrivatarInformation parameters (20261019120300).';
