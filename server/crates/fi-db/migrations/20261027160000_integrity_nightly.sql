-- Hybrid Hardcore checks (owner 2026-10-26, owner-decisions "no outbox or BigQuery; hybrid
-- Hardcore checks"; CLOUD_SAVES.md §7.5, SAVES_API.md §4.3/§8). A Hardcore upload is refused at
-- once only by the cheap, deterministic checks (unreadable numbers, a career a reset replaced, the
-- game's limits, the fresh-start check, the fact checks); the guardrails' rate replay, ceilings and
-- anomaly score move to the nightly `fi-cli integrity-nightly`, which assesses every revision the
-- inline path no longer assesses, exactly as the inline path did (fi_saves::service::guardrails).
-- Expand only: instances of the previous build never read or write the new columns (they keep
-- assessing inline and commit with guard_input NULL, which the nightly does not queue).
--
-- 1. save_revision:
--    - guard_input: what the nightly needs beyond the row and the line's history, recorded by the
--      commit of a gated player upload (`upload`, `resolve_keep_local` on a Hardcore line): the
--      profile's last accepted game sync the upload was compared with and the play time it was
--      measured at, as read at the upload
--      ({"game": null | {"credits", "xp", "level", "hoursSince", "hoursSinceBits"}, "playMs"}:
--      the bits are the exact f64 the upload measured with, playMs the line's play time the upload
--      was measured at). NULL: not queued
--      (every other kind, every Noob commit, every revision from before this migration, which the
--      inline path assessed).
--    - guard_analyzed_at: set once by the nightly when it recorded the revision's assessment, never
--      cleared. Queued = guard_input IS NOT NULL AND guard_analyzed_at IS NULL; retention keeps
--      the bundle of every queued revision (fi_db::save_retention::kept_ctes, `unanalyzed`).
--      The marker lives on the row, not in integrity_assessment, whose rows data-retention deletes
--      after 90 days: an old revision never re-enters the queue.
ALTER TABLE save_revision
  ADD COLUMN guard_input jsonb CHECK (jsonb_typeof(guard_input) = 'object'),
  ADD COLUMN guard_analyzed_at timestamptz,
  ADD CONSTRAINT save_revision_guard_shape
    CHECK (guard_analyzed_at IS NULL OR guard_input IS NOT NULL);

-- Revisions are immutable except `pinned`, the one-way `bundle_pruned_at` (20261026170000) and the
-- one-way `guard_analyzed_at`.
CREATE OR REPLACE FUNCTION fi_save_revision_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - 'pinned' - 'bundle_pruned_at' - 'guard_analyzed_at')
     IS DISTINCT FROM (to_jsonb(OLD) - 'pinned' - 'bundle_pruned_at' - 'guard_analyzed_at') THEN
    RAISE EXCEPTION 'save_revision is immutable except pinned' USING ERRCODE = 'restrict_violation';
  END IF;
  IF OLD.bundle_pruned_at IS NOT NULL AND NEW.bundle_pruned_at IS DISTINCT FROM OLD.bundle_pruned_at THEN
    RAISE EXCEPTION 'a pruned bundle stays pruned' USING ERRCODE = 'restrict_violation';
  END IF;
  IF OLD.guard_analyzed_at IS NOT NULL AND NEW.guard_analyzed_at IS DISTINCT FROM OLD.guard_analyzed_at THEN
    RAISE EXCEPTION 'an analyzed revision stays analyzed' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

-- A new revision has its bundle and is not analyzed yet.
CREATE OR REPLACE FUNCTION fi_save_revision_insert_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.bundle_pruned_at IS NOT NULL THEN
    RAISE EXCEPTION 'a new revision has its bundle' USING ERRCODE = 'restrict_violation';
  END IF;
  IF NEW.guard_analyzed_at IS NOT NULL THEN
    RAISE EXCEPTION 'a new revision is not analyzed yet' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

-- Only the nightly marks a revision analyzed (fi_runtime has no UPDATE on the column).
GRANT UPDATE (guard_analyzed_at) ON save_revision TO fi_maintenance;

-- The nightly's queue: the lines with a revision not analyzed yet.
CREATE INDEX save_revision_unanalyzed ON save_revision (profile_id, revision)
  WHERE guard_input IS NOT NULL AND guard_analyzed_at IS NULL;

-- 2. integrity_assessment:
--    - source `save_nightly`: the nightly's assessment of one revision, with `revision` (one per
--      revision: the unique index below is the job's idempotency);
--    - decision `flagged`: the nightly found what the inline guardrails would have refused (a
--      ceiling whatever the switch; a score of 60 or more with FI_GUARD_ENFORCE on), with the rule
--      in `refusal`. Nothing is rolled back: the revision stays the line's, the row is the
--      moderation queue's entry (GET /core/v1/admin/assessments). Only the nightly flags, and the
--      nightly never refuses.
ALTER TABLE integrity_assessment DROP CONSTRAINT integrity_assessment_source_check;
ALTER TABLE integrity_assessment DROP CONSTRAINT integrity_assessment_audit_shape;
ALTER TABLE integrity_assessment DROP CONSTRAINT integrity_assessment_decision;
ALTER TABLE integrity_assessment DROP CONSTRAINT integrity_assessment_decision_check;
ALTER TABLE integrity_assessment
  ADD CONSTRAINT integrity_assessment_source_check
    CHECK (source IN ('save_commit', 'profile_sync', 'tables_sync', 'save_audit', 'save_nightly')),
  ADD CONSTRAINT integrity_assessment_audit_shape
    CHECK ((source = 'save_audit') = (audit IS NOT NULL)
       AND (source IN ('save_audit', 'save_nightly')) = (revision IS NOT NULL)),
  ADD CONSTRAINT integrity_assessment_decision_check
    CHECK (decision IN ('accepted', 'refused', 'flagged')),
  ADD CONSTRAINT integrity_assessment_decision
    CHECK ((decision IN ('refused', 'flagged')) = (refusal IS NOT NULL)),
  ADD CONSTRAINT integrity_assessment_nightly_flags
    CHECK ((decision = 'flagged') <= (source = 'save_nightly')
       AND (source = 'save_nightly') <= (decision <> 'refused'));

CREATE UNIQUE INDEX integrity_assessment_nightly
  ON integrity_assessment (profile_id, revision) WHERE source = 'save_nightly';

-- The nightly records (as a fi_maintenance login, fi_jobs); retention and erasure still delete.
GRANT INSERT ON integrity_assessment TO fi_maintenance;
