-- OD-67, ADR 0021 (CLOUD_SAVES.md §7.3): the save audit. A fi-cli job (`saves-audit`) reads the
-- PlayerDatabase of accepted Hardcore revisions with fi-playerdb and records one shadow
-- assessment per revision: source `save_audit`, the revision it read, and the audit's numbers
-- (`audit`: fi_gamedb::audit::Facts, camelCase). Never a refusal: the upload path is unchanged.
--
-- Expand only: rows of the other sources keep `revision` and `audit` NULL, so the instances of a
-- rolling deploy on the previous build keep inserting as before.
ALTER TABLE integrity_assessment DROP CONSTRAINT integrity_assessment_source_check;
ALTER TABLE integrity_assessment
  ADD CONSTRAINT integrity_assessment_source_check
    CHECK (source IN ('save_commit', 'profile_sync', 'tables_sync', 'save_audit')),
  ADD COLUMN revision bigint CHECK (revision > 0),
  ADD COLUMN audit jsonb CHECK (jsonb_typeof(audit) = 'object'),
  ADD CONSTRAINT integrity_assessment_audit_shape
    CHECK ((source = 'save_audit') = (revision IS NOT NULL AND audit IS NOT NULL)
       AND num_nulls(revision, audit) IN (0, 2)),
  -- The audit never refuses.
  ADD CONSTRAINT integrity_assessment_audit_accepts
    CHECK (source <> 'save_audit' OR decision = 'accepted');

-- One audit per revision: the job's idempotency (a rerun or a second job inserts nothing) and its
-- queue's anti-join.
CREATE UNIQUE INDEX integrity_assessment_audited
  ON integrity_assessment (profile_id, revision) WHERE source = 'save_audit';

-- The audit's queue walks the revisions of the last days (integrity::audit_due_in; the only
-- committed_at index so far is partial, NOT pinned).
CREATE INDEX save_revision_committed ON save_revision (committed_at);
