-- Save retention (owner 2026-10-26 "keep 10 save revisions per line"; SAVES_API.md §8,
-- CLOUD_SAVES.md §6 and §8, fi_db::save_retention). Expand only: instances of the previous build
-- never read or write the new columns.
--
-- 1. Retention prunes a revision's save file, never its row: the row (numbers, digests, kind, play
--    time) stays for the guardrails, the replaced-career check and the history's contiguity.
--    `bundle_pruned_at` says the bundle is no longer kept: the revision is not downloaded or
--    restored again, and its object is garbage once nothing unpruned names it. Set once, never
--    cleared (the guard below).
ALTER TABLE save_revision ADD COLUMN bundle_pruned_at timestamptz;

-- Revisions are immutable except `pinned` and the one-way `bundle_pruned_at`
-- (20261017000400_saves.sql's guard, plus the pruning).
CREATE OR REPLACE FUNCTION fi_save_revision_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - 'pinned' - 'bundle_pruned_at') IS DISTINCT FROM (to_jsonb(OLD) - 'pinned' - 'bundle_pruned_at') THEN
    RAISE EXCEPTION 'save_revision is immutable except pinned' USING ERRCODE = 'restrict_violation';
  END IF;
  IF OLD.bundle_pruned_at IS NOT NULL AND NEW.bundle_pruned_at IS DISTINCT FROM OLD.bundle_pruned_at THEN
    RAISE EXCEPTION 'a pruned bundle stays pruned' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

-- fi_runtime inserts revisions (a table-level grant): a new revision always has its bundle.
CREATE FUNCTION fi_save_revision_insert_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.bundle_pruned_at IS NOT NULL THEN
    RAISE EXCEPTION 'a new revision has its bundle' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER save_revision_insert_guard BEFORE INSERT ON save_revision FOR EACH ROW EXECUTE FUNCTION fi_save_revision_insert_guard();

-- Only retention prunes (fi_runtime has no UPDATE on the column).
GRANT UPDATE (bundle_pruned_at) ON save_revision TO fi_maintenance;

-- 2. The garbage collection deletes the object of a blob only pruned revisions name, but their
--    foreign key keeps the row: `object_deleted_at` records that the object is gone (the row stays
--    marked). A commit, park or seed import that needs the row again clears both and puts the
--    object back (fi_db::saves::insert_blob), so the runtime may clear it too.
ALTER TABLE save_blob ADD COLUMN object_deleted_at timestamptz;
ALTER TABLE save_blob ADD CONSTRAINT save_blob_deleted_is_marked
  CHECK (object_deleted_at IS NULL OR gc_marked_at IS NOT NULL) NOT VALID;
ALTER TABLE save_blob VALIDATE CONSTRAINT save_blob_deleted_is_marked;

-- A blob row is immutable except its garbage-collection columns; only a fi_maintenance member (the
-- collector) or the owner records a deleted object; the runtime may only clear it (the upsert).
CREATE OR REPLACE FUNCTION fi_save_blob_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - 'gc_marked_at' - 'object_deleted_at') IS DISTINCT FROM (to_jsonb(OLD) - 'gc_marked_at' - 'object_deleted_at') THEN
    RAISE EXCEPTION 'save_blob is immutable except gc_marked_at' USING ERRCODE = 'restrict_violation';
  END IF;
  IF NEW.object_deleted_at IS NOT NULL AND NEW.object_deleted_at IS DISTINCT FROM OLD.object_deleted_at
     AND NOT pg_has_role(current_user, 'fi_maintenance', 'MEMBER') THEN
    RAISE EXCEPTION 'only the garbage collection records a deleted object' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

-- A new blob row has its object (the caller put it first).
CREATE FUNCTION fi_save_blob_insert_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.object_deleted_at IS NOT NULL THEN
    RAISE EXCEPTION 'a new blob row has its object' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER save_blob_insert_guard BEFORE INSERT ON save_blob FOR EACH ROW EXECUTE FUNCTION fi_save_blob_insert_guard();

GRANT UPDATE (object_deleted_at) ON save_blob TO fi_runtime, fi_maintenance;

-- 3. Retention keeps an expired parked bundle while an integrity strike names it (moderation
--    evidence; the foreign key would refuse the delete anyway). The job reads that one column of
--    the strike and nothing else.
GRANT SELECT (parked_id) ON integrity_strike TO fi_maintenance;
