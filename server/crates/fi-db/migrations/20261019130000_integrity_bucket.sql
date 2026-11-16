-- SER-M3 step 6 review (GAME_PLATFORM.md §3a "Supporter universe", RPC gates): the growth gates
-- are token buckets, one per metric. The base allowance is the bucket's size (the most at once),
-- refilled at the hourly rate up to that size; each accepted sync spends its growth from what is
-- left, a decrease puts it back up to the size, and seeding fills every bucket. The Beta's
-- per-sync allowance gave every accepted sync the base again, so N back-to-back syncs added N x
-- the base (40 syncs within a second: +21.6M credits).
--
-- What is left of each bucket after the last accepted sync of its half; the refill time is that
-- half's `synced_at` / `tables_synced_at`, which every accepted sync of the half moves. NULL is a
-- full bucket (rows written before this migration, and a half not seeded yet).
-- 20261019120200_integrity.sql stays as it was: test databases have applied it.
ALTER TABLE integrity_baseline
  ADD COLUMN allowance_credits  bigint  CHECK (allowance_credits >= 0),
  ADD COLUMN allowance_winnings bigint  CHECK (allowance_winnings >= 0),
  ADD COLUMN allowance_xp       bigint  CHECK (allowance_xp >= 0),
  ADD COLUMN allowance_level    bigint  CHECK (allowance_level >= 0),
  ADD COLUMN allowance_cars     integer CHECK (allowance_cars >= 0);
