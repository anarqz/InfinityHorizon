-- The review of OD-62's guardrails (docs/server/REVIEW_LOG.md, "OD-62 guardrails review", G2-1 to
-- G2-14; ADR 0020, CLOUD_SAVES.md §7.3). Expand only: every new column is NULL for the rows of
-- 20261019194500_guard.sql and NULL reads as "none" (a rate of 0, no mark, nothing judged), so the
-- instances of a rolling deploy still on the previous build keep working.
--
-- 1. integrity_baseline, profile half (the game's UpdateAll):
--    - rate_day_*: the slow rates (an exponentially weighted moving average with a 24-hour time
--      constant over the whole gap since the last accepted sync), with their own ceilings, which
--      bound play sustained just under the fast ceilings (an hourly sync of ~49M; G2-1);
--    - level_high: the highest level accepted since the seed; level-up spins are granted only
--      above it, so lowering the level and gaining it back earns nothing (G2-3);
--    - judged_*: the fact checks' values a refusal by score judged since the last accepted sync
--      (CLOUD_SAVES.md §7.1 rule 4: a value is refused once; G2-6): the largest level deviation
--      from the XP curve, and the lowest XP and level a drop was refused at. Cleared, with the
--      refusal episode, by the next accepted sync.
ALTER TABLE integrity_baseline
  ADD COLUMN rate_day_unexplained double precision CHECK (rate_day_unexplained >= 0 AND rate_day_unexplained < 'Infinity'::double precision),
  ADD COLUMN rate_day_winnings    double precision CHECK (rate_day_winnings >= 0 AND rate_day_winnings < 'Infinity'::double precision),
  ADD COLUMN rate_day_xp          double precision CHECK (rate_day_xp >= 0 AND rate_day_xp < 'Infinity'::double precision),
  ADD COLUMN rate_day_levels      double precision CHECK (rate_day_levels >= 0 AND rate_day_levels < 'Infinity'::double precision),
  ADD COLUMN level_high           bigint CHECK (level_high >= 0),
  ADD COLUMN judged_level_deviation bigint CHECK (judged_level_deviation > 0),
  ADD COLUMN judged_drop_xp       bigint CHECK (judged_drop_xp >= 0),
  ADD COLUMN judged_drop_level    bigint CHECK (judged_drop_level >= 0),
  ADD CONSTRAINT integrity_baseline_judged_drop_shape
    CHECK (num_nulls(judged_drop_xp, judged_drop_level) IN (0, 2));

-- 2. The assessments are evidence: a Hardcore -> Noob change keeps them (as it keeps
-- integrity_strike), so a profile cannot erase its record by changing mode (G2-9). Retention stays
-- 90 days (a follow-up job, not built); erasure (fi_maintenance) still deletes them.
DELETE FROM cloud_purge_target WHERE table_name = 'integrity_assessment'::regclass;

-- 3. The guardrails' rate window (fi_db::saves play_window_start) looks for the newest readable
-- revision below a play time: on the line and its play time (G2-12).
CREATE INDEX IF NOT EXISTS save_revision_play ON save_revision (profile_id, play_ms);
