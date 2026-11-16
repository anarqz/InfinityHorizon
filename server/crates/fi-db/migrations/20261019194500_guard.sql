-- OD-62, the Hardcore guardrails (ADR 0020; CLOUD_SAVES.md §7.3, GAME_PLATFORM.md §3a "RPC
-- gates"): a Hardcore save and the game's syncs are untrusted input. Ceilings refuse absurd
-- values; rates and conservation gaps only score (0-100 with reason codes, fi_guard) and refuse
-- only when the deployment's FI_GUARD_ENFORCE is on. Every assessment is a row, so the shadow
-- phase can be calibrated (P95/P99) and every decision explained to a moderator.
--
-- 1. integrity_baseline: the token buckets refused legitimate bursts and no longer decide. Each
-- half carries instead the guardrails' rates at its last accepted sync, per hour (an
-- exponentially weighted moving average with a one-hour time constant, exact for any sync
-- cadence): credits, unexplained credits (beyond winnings and the level-up spin allowance),
-- winnings, XP and levels on the profile half, cars on the garage half. NULL is 0, no recent
-- growth: a seeded half and every row from before this migration. The times stay `synced_at` /
-- `tables_synced_at`. Expand, then contract: the buckets' `allowance_*` columns
-- (20261019130000_integrity_bucket.sql) stay, unused by this release, so the instances of a
-- rolling deploy still running the previous build keep working; a later migration drops them.
ALTER TABLE integrity_baseline
  ADD COLUMN rate_credits     double precision CHECK (rate_credits >= 0 AND rate_credits < 'Infinity'::double precision),
  ADD COLUMN rate_unexplained double precision CHECK (rate_unexplained >= 0 AND rate_unexplained < 'Infinity'::double precision),
  ADD COLUMN rate_winnings    double precision CHECK (rate_winnings >= 0 AND rate_winnings < 'Infinity'::double precision),
  ADD COLUMN rate_xp          double precision CHECK (rate_xp >= 0 AND rate_xp < 'Infinity'::double precision),
  ADD COLUMN rate_levels      double precision CHECK (rate_levels >= 0 AND rate_levels < 'Infinity'::double precision),
  ADD COLUMN rate_cars        double precision CHECK (rate_cars >= 0 AND rate_cars < 'Infinity'::double precision);

-- 2. One row per assessed Hardcore measurement (the study's LOG step): an accepted or refused
-- profile sync (OnlineProfile.UpdateAll), garage upload (UpdateAllTables carrying a garage) or
-- save commit. A replayed request or commit writes none. `game` first (ADR 0902 §1).
--
-- - play_session: the play session of the request (no foreign key: sessions are pruned on their
--   own schedule, an assessment outlives them);
-- - hours: what was measured (wall time between syncs; play time between save revisions);
-- - deltas: {credits, xp, levels, winnings, cars, unexplained, game?} (null where a side lacks the
--   number; `game` is a save against the profile's last accepted sync: {credits, xp, levels,
--   hoursSince}); rates: {credits, unexplained, winnings, xp, levels, cars} per hour after it;
-- - reasons: fi_guard::ReasonCode codes; refusal: the refusing rule's code (a ceiling's, or
--   ANOMALY_SCORE with the switch on), NULL when accepted; would_block: score >= 60;
-- - rules_version: fi_guard::RULES_VERSION.
--
-- No IP, no e-mail, no gamertag: the profile id names the player, as everywhere.
CREATE TABLE integrity_assessment (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  game          text NOT NULL REFERENCES game (code),
  profile_id    uuid NOT NULL REFERENCES profile (id),
  mode          text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  play_session  uuid NOT NULL,
  source        text NOT NULL CHECK (source IN ('save_commit', 'profile_sync', 'tables_sync')),
  hours         double precision NOT NULL CHECK (hours >= 0 AND hours <= 720),
  deltas        jsonb NOT NULL CHECK (jsonb_typeof(deltas) = 'object'),
  rates         jsonb NOT NULL CHECK (jsonb_typeof(rates) = 'object'),
  score         smallint NOT NULL CHECK (score BETWEEN 0 AND 100),
  reasons       text[] NOT NULL DEFAULT '{}'
                  CHECK (cardinality(reasons) <= 32 AND array_position(reasons, NULL) IS NULL),
  refusal       text CHECK (refusal ~ '^[A-Z][A-Z0-9_]{0,63}$'),
  decision      text NOT NULL CHECK (decision IN ('accepted', 'refused')),
  would_block   boolean NOT NULL,
  rules_version text NOT NULL CHECK (rules_version ~ '^[a-z0-9.-]{1,32}$'),
  at            timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT integrity_assessment_decision CHECK ((decision = 'refused') = (refusal IS NOT NULL))
);

-- A profile's assessments newest first (the moderator read, the purge's (game, profile_id)); the
-- game's feed newest first (the moderation queue by minScore).
CREATE INDEX integrity_assessment_profile ON integrity_assessment (game, profile_id, id DESC);
CREATE INDEX integrity_assessment_feed ON integrity_assessment (game, id DESC);

-- The runtime records and reads; it never changes or deletes one.
GRANT SELECT, INSERT ON integrity_assessment TO fi_runtime;
-- Retention (90 days, a follow-up job, not built) and erasure delete them.
GRANT SELECT, DELETE ON integrity_assessment TO fi_maintenance;

-- Hardcore cloud data (OD-52): a Hardcore -> Noob change deletes the profile's assessments with
-- the rest of its cloud data (not shown in the warning).
INSERT INTO cloud_purge_target (table_name, column_name, category) VALUES
  ('integrity_assessment', 'profile_id', NULL);
