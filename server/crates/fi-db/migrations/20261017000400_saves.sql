-- ForzaInfinity schema, part 4 of 5: cloud saves v2 (docs/server/CLOUD_SAVES.md §10, ADR 0005).
--
-- save_head is the one row per profile that every save write locks (SELECT ... FOR UPDATE): the
-- lease check, the revision compare-and-swap and the new revision commit in one transaction
-- (crates/fi-db/src/saves.rs). Revisions are immutable; the head points at its revision through a
-- deferred foreign key, so it can never name a revision that does not exist, and retention can
-- never delete the head's revision. A trigger refuses, whoever writes the row, every head change
-- the protocol forbids (a token or the head going back).

-- Content-addressed bundles. The object `blobs/sha256/<hex>` in the cloud-saves bucket is written
-- with ifGenerationMatch=0 (create-only) before its row exists. Objects are shared by every row
-- that names the same digest (every Hardcore fresh career of one starter car is one object), so
-- nothing deletes an object but the two-phase garbage collection (CLOUD_SAVES.md §6):
--   1. mark: an unreferenced row gets gc_marked_at (orphan objects first get a marked row);
--   2. any commit or park that needs the blob again clears the mark (insert ... ON CONFLICT DO
--      UPDATE, which also locks the row);
--   3. at least 24 h later, under the row's lock and only if still marked and unreferenced, the
--      job deletes the object, then the row. A commit that then creates the row again re-puts the
--      object (fi_saves Committed::blob_created).
-- Erasure deletes rows, never objects.
CREATE TABLE save_blob (
  sha256       bytea PRIMARY KEY CHECK (length(sha256) = 32),
  size_bytes   integer NOT NULL CHECK (size_bytes BETWEEN 1 AND 16777216),
  format       text NOT NULL DEFAULT 'fh2save1' CHECK (format IN ('fh2save1')),
  storage_key  text NOT NULL CONSTRAINT save_blob_storage_key UNIQUE,
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  gc_marked_at timestamptz,
  CONSTRAINT save_blob_key_is_the_digest CHECK (storage_key = 'blobs/sha256/' || encode(sha256, 'hex'))
);

-- A blob row is immutable except its garbage-collection mark.
CREATE FUNCTION fi_save_blob_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - 'gc_marked_at') IS DISTINCT FROM (to_jsonb(OLD) - 'gc_marked_at') THEN
    RAISE EXCEPTION 'save_blob is immutable except gc_marked_at' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER save_blob_immutable BEFORE UPDATE ON save_blob FOR EACH ROW EXECUTE FUNCTION fi_save_blob_guard();
CREATE INDEX save_blob_gc ON save_blob (gc_marked_at) WHERE gc_marked_at IS NOT NULL;

-- Bundles kept outside the line: conflict losers, gate refusals, unreadable saves, pre-reset
-- copies. Retention drops them at expires_at (90 days by default), except one a revision names
-- (parked_accept), which is kept as long as that revision.
CREATE TABLE save_parked (
  id                uuid PRIMARY KEY DEFAULT uuidv7(),
  profile_id        uuid NOT NULL REFERENCES profile (id),
  blob_sha256       bytea NOT NULL REFERENCES save_blob (sha256),
  content_sha256    bytea NOT NULL CHECK (length(content_sha256) = 32),
  reason            text NOT NULL CHECK (reason IN ('conflict_loser', 'gate_refused', 'unreadable', 'pre_restore', 'pre_reset')),
  based_on_revision bigint CHECK (based_on_revision >= 0),
  session_id        uuid,
  device_id         uuid,
  summary           jsonb CHECK (summary IS NULL OR jsonb_typeof(summary) = 'object'),
  detail            text NOT NULL CHECK (char_length(detail) <= 500),
  parked_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at        timestamptz NOT NULL,
  CONSTRAINT save_parked_expiry CHECK (expires_at > parked_at)
);

CREATE TRIGGER save_parked_append_only BEFORE UPDATE ON save_parked FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();
CREATE INDEX save_parked_profile ON save_parked (profile_id, parked_at);
CREATE INDEX save_parked_expiry ON save_parked (expires_at);
CREATE INDEX save_parked_blob ON save_parked (blob_sha256);

CREATE TABLE save_revision (
  profile_id     uuid NOT NULL REFERENCES profile (id),
  revision       bigint NOT NULL CHECK (revision > 0),
  commit_id      uuid NOT NULL CONSTRAINT save_revision_commit_id_key UNIQUE,
  blob_sha256    bytea NOT NULL REFERENCES save_blob (sha256),
  -- SHA-256 over the data files (fi_saves::bundle::content_digest): equal saves, equal digest,
  -- whatever folder label or packing the bundle has. Sync decisions compare this.
  content_sha256 bytea NOT NULL CHECK (length(content_sha256) = 32),
  kind           text NOT NULL CHECK (kind IN ('upload', 'resolve_keep_local', 'restore', 'fresh_career', 'import_offline', 'beta_import', 'moderation_reset', 'season_reset', 'career_reset', 'parked_accept')),
  actor          text NOT NULL CHECK (actor IN ('player', 'system', 'moderator', 'import')),
  session_id     uuid NOT NULL,
  fence_token    bigint NOT NULL CHECK (fence_token > 0),
  device_id      uuid,
  client_label   text CHECK (client_label ~ '^[0-9A-F]{16}$'),
  restored_from  bigint,
  -- The parked bundle a moderator accepted (kind parked_accept).
  parked_from    uuid REFERENCES save_parked (id),
  summary        jsonb NOT NULL CHECK (jsonb_typeof(summary) = 'object'),
  committed_at   timestamptz NOT NULL,
  -- The line's play time at this commit (save_head.play_ms): the Hardcore gates measure the hours
  -- between two revisions as the difference.
  play_ms        bigint NOT NULL CHECK (play_ms >= 0),
  pinned         boolean NOT NULL DEFAULT false,
  PRIMARY KEY (profile_id, revision),
  CONSTRAINT save_revision_restore_source CHECK (
    (kind = 'restore') = (restored_from IS NOT NULL) AND (restored_from IS NULL OR (restored_from > 0 AND restored_from < revision))
  ),
  CONSTRAINT save_revision_parked_source CHECK ((kind = 'parked_accept') = (parked_from IS NOT NULL)),
  -- Who may write what (fi_saves CommitKind::allows says the same).
  CONSTRAINT save_revision_kind_actor CHECK (
    CASE kind
      WHEN 'upload' THEN actor = 'player'
      WHEN 'resolve_keep_local' THEN actor = 'player'
      WHEN 'import_offline' THEN actor = 'player'
      WHEN 'restore' THEN actor IN ('player', 'moderator')
      WHEN 'fresh_career' THEN actor = 'system'
      WHEN 'beta_import' THEN actor = 'import'
      WHEN 'moderation_reset' THEN actor = 'moderator'
      WHEN 'parked_accept' THEN actor = 'moderator'
      ELSE actor IN ('system', 'moderator')
    END
  )
);

-- Revisions are immutable; only `pinned` (retention exemption) may change.
CREATE FUNCTION fi_save_revision_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - 'pinned') IS DISTINCT FROM (to_jsonb(OLD) - 'pinned') THEN
    RAISE EXCEPTION 'save_revision is immutable except pinned' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER save_revision_immutable BEFORE UPDATE ON save_revision FOR EACH ROW EXECUTE FUNCTION fi_save_revision_guard();
CREATE INDEX save_revision_blob ON save_revision (blob_sha256);
CREATE INDEX save_revision_retention ON save_revision (committed_at) WHERE NOT pinned;
CREATE INDEX save_revision_parked ON save_revision (parked_from) WHERE parked_from IS NOT NULL;

CREATE TABLE save_head (
  profile_id         uuid PRIMARY KEY REFERENCES profile (id),
  -- 0: no save yet. Moves by exactly one per commit (the CAS in saves.rs; the guard below).
  revision           bigint NOT NULL DEFAULT 0 CHECK (revision >= 0),
  head_revision      bigint GENERATED ALWAYS AS (NULLIF(revision, 0)) STORED,
  -- Highest fencing token ever granted; the next grant is fence_seq + 1.
  fence_seq          bigint NOT NULL DEFAULT 0 CHECK (fence_seq >= 0),
  -- The newest reset revision (0: never reset). Players may not restore anything older, nor
  -- re-upload a career it replaced; moderators may (CLOUD_SAVES.md §8).
  reset_revision     bigint NOT NULL DEFAULT 0 CHECK (reset_revision >= 0),
  -- Play time: milliseconds some session held a live lease on the line, credited on every grant,
  -- heartbeat, commit and release up to lease_accounted_at (fi_saves::lease::play_credit). The
  -- Hardcore gates measure allowances on it, never on wall-clock time.
  play_ms            bigint NOT NULL DEFAULT 0 CHECK (play_ms >= 0),
  -- The current lease (all NULL when released). It may be past lease_expires_at: expiry is
  -- evaluated against the database clock by whoever reads it under the row lock.
  lease_session_id   uuid,
  lease_token        bigint,
  lease_device_id    uuid,
  lease_device_label text CHECK (char_length(lease_device_label) BETWEEN 1 AND 64),
  lease_acquired_at  timestamptz,
  lease_expires_at   timestamptz,
  lease_ttl_ms       integer CHECK (lease_ttl_ms BETWEEN 1000 AND 3600000),
  lease_accounted_at timestamptz,
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version        bigint NOT NULL DEFAULT 1,
  CONSTRAINT save_head_points_at_its_revision FOREIGN KEY (profile_id, head_revision)
    REFERENCES save_revision (profile_id, revision) DEFERRABLE INITIALLY DEFERRED,
  CONSTRAINT save_head_lease_shape CHECK (
    num_nulls(lease_session_id, lease_token, lease_device_id, lease_device_label, lease_acquired_at, lease_expires_at, lease_ttl_ms, lease_accounted_at) IN (0, 8)
  ),
  -- The current lease always carries the newest token.
  CONSTRAINT save_head_lease_is_newest CHECK (lease_token IS NULL OR lease_token = fence_seq),
  CONSTRAINT save_head_reset_is_a_revision CHECK (reset_revision <= revision)
);

-- What the protocol forbids, refused whatever writes the row (services, jobs, the importer, an
-- operator's psql): the fence sequence going back (with lease_token = fence_seq, an older token
-- would become current again), the head moving other than to itself or the next revision (a lost
-- update: the deferred foreign key alone accepts any existing revision), the reset revision or the
-- play time going back. The one exception is erasure: once the profile is deleted, the head and
-- its reset revision return to 0 so the revisions can be deleted (ACCOUNTS.md §8).
CREATE FUNCTION fi_save_head_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.fence_seq < OLD.fence_seq THEN
    RAISE EXCEPTION 'save_head.fence_seq never goes back (% -> %)', OLD.fence_seq, NEW.fence_seq
      USING ERRCODE = 'restrict_violation';
  END IF;
  IF NEW.play_ms < OLD.play_ms THEN
    RAISE EXCEPTION 'save_head.play_ms never goes back' USING ERRCODE = 'restrict_violation';
  END IF;
  IF NEW.revision NOT IN (OLD.revision, OLD.revision + 1) OR NEW.reset_revision < OLD.reset_revision THEN
    IF NEW.revision = 0 AND NEW.reset_revision = 0
       AND EXISTS (SELECT 1 FROM profile p WHERE p.id = NEW.profile_id AND p.status = 'deleted') THEN
      RETURN NEW;
    END IF;
    RAISE EXCEPTION 'save_head moves to the next revision or stays (% -> %)', OLD.revision, NEW.revision
      USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER save_head_guard BEFORE UPDATE ON save_head FOR EACH ROW EXECUTE FUNCTION fi_save_head_guard();
CREATE TRIGGER save_head_touch BEFORE UPDATE ON save_head FOR EACH ROW EXECUTE FUNCTION fi_touch_row();

-- The save audit trail: every lease grant, takeover, resume, release, commit and refused commit,
-- with tokens and revisions, so support can answer "what happened to my save" from facts.
CREATE TABLE save_event (
  id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  profile_id  uuid NOT NULL REFERENCES profile (id),
  kind        text NOT NULL CHECK (kind IN ('lease_granted', 'lease_taken_over', 'lease_resumed', 'lease_released', 'commit', 'commit_refused', 'parked', 'restore', 'download', 'installed', 'pruned')),
  session_id  uuid,
  fence_token bigint,
  revision    bigint,
  detail      jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(detail) = 'object'),
  occurred_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER save_event_append_only BEFORE UPDATE ON save_event FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();
CREATE INDEX save_event_profile ON save_event (profile_id, occurred_at);

-- Hardcore refusals, reviewable by a moderator (Beta kept a counter and the last reason only).
CREATE TABLE integrity_strike (
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  profile_id  uuid NOT NULL REFERENCES profile (id),
  source      text NOT NULL CHECK (source IN ('save_gate', 'profile_gate', 'race_time', 'top_speed')),
  reason      text NOT NULL CHECK (char_length(reason) BETWEEN 1 AND 500),
  parked_id   uuid REFERENCES save_parked (id),
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  reviewed_at timestamptz,
  reviewed_by text CHECK (char_length(reviewed_by) <= 128),
  outcome     text CHECK (outcome IN ('upheld', 'overturned')),
  CONSTRAINT integrity_strike_review_shape CHECK (num_nulls(reviewed_at, reviewed_by, outcome) IN (0, 3))
);

CREATE INDEX integrity_strike_profile ON integrity_strike (profile_id, created_at);

GRANT SELECT, INSERT ON save_blob, save_revision, save_parked, save_event, integrity_strike TO fi_runtime;
-- A commit or park that needs a marked blob clears the mark (ON CONFLICT DO UPDATE).
GRANT UPDATE (gc_marked_at) ON save_blob TO fi_runtime;
GRANT SELECT, INSERT, UPDATE ON save_head TO fi_runtime;
GRANT UPDATE (pinned) ON save_revision TO fi_runtime;
GRANT UPDATE (reviewed_at, reviewed_by, outcome) ON integrity_strike TO fi_runtime;
-- Retention and garbage collection (keep N revisions + pinned + the head; expired parked bundles;
-- blobs marked, then deleted 24 h later if still unreferenced).
GRANT SELECT, DELETE ON save_blob, save_revision, save_parked TO fi_maintenance;
GRANT UPDATE (gc_marked_at) ON save_blob TO fi_maintenance;
GRANT SELECT, INSERT ON save_event TO fi_maintenance;
GRANT SELECT ON save_head TO fi_maintenance;
