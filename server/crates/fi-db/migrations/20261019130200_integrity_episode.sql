-- SER-M3 step 6 review (GAME_PLATFORM.md §3a "Supporter universe", RPC gates): a Hardcore profile
-- is struck once per refusal episode, not once per refused request. A growth above a bucket's
-- size never fits, so the game's every later sync carrying it is refused too; striking each one
-- would pile up tens of strikes a session for one event.
--
-- Per half and mode: when the open episode began (the first refused sync of the half since its
-- last accepted one), NULL when none is open. The first refusal of an episode sets it and strikes;
-- later refusals find it set and only record their fact; the next accepted sync of the half clears
-- it. A refusal before any baseline creates the row with both halves still NULL (the shape CHECKs
-- allow it). 20261019130000_integrity_bucket.sql stays as it was.
ALTER TABLE integrity_baseline
  ADD COLUMN profile_refused_at timestamptz,
  ADD COLUMN garage_refused_at  timestamptz;
