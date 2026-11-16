-- Liveries are stored on the server again (owner decision 2026-10-27, "Liveries are stored on the
-- server again; the live lobby sync retires"; GAME_PLATFORM.md §3a "Liveries", register CR-S59).
-- Supersedes OD-65's temporary rows: every ticket's paint is now kept per profile and car, so the
-- id the game stores on the car (car+0x258) stays valid across sessions without the hook's
-- pre-join upload or the relay's live sync. Written only through fi_db::liveries.
--
-- 1. The design bytes, content-addressed per profile: the game sends the same design again on
--    every apply (UpdateLivery, then CreateVersionedLivery), so one row per (profile, sha256)
--    holds them however many drafts and versions name them. Kept in Postgres, not the bucket: the
--    matchmaking deployment (free tickets) has no bucket and must serve any world mate's paint.
--    A row no draft or livery names is deleted in the transaction that dropped its last
--    reference (fi_db::liveries::release_in), unless a moderator hid it (kept, so the same bytes
--    applied again stay hidden), and by erasure (step `livery_blob`). The account's bytes are
--    bounded by FI_LIVERY_ACCOUNT_BYTES (fi_db::liveries::save_draft_in).
CREATE TABLE livery_blob (
  -- The game the profile plays (rows are per profile; kept for the per-game reads).
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  sha256     bytea NOT NULL CHECK (length(sha256) = 32),
  -- VersionedLiveryData.data's max_len in the game's schema: 512 KiB.
  size       integer NOT NULL CHECK (size BETWEEN 1 AND 524288),
  data       bytea NOT NULL CHECK (octet_length(data) = size),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- Moderation: hidden bytes stay hidden whatever livery names them.
  hidden_at  timestamptz,
  PRIMARY KEY (profile_id, sha256)
);

-- 2. Drafts and versioned liveries name their bytes; rows written before this migration keep
--    their in-row `data` and stay readable (no backfill: those were temporary or few).
ALTER TABLE game_livery_draft ALTER COLUMN data DROP NOT NULL;
ALTER TABLE game_livery_draft ADD COLUMN blob_sha256 bytea;
ALTER TABLE game_livery_draft ADD CONSTRAINT game_livery_draft_blob
  FOREIGN KEY (profile_id, blob_sha256) REFERENCES livery_blob (profile_id, sha256);
ALTER TABLE game_livery_draft ADD CONSTRAINT game_livery_draft_bytes
  CHECK (data IS NOT NULL OR blob_sha256 IS NOT NULL);
CREATE INDEX game_livery_draft_blob ON game_livery_draft (profile_id, blob_sha256)
  WHERE blob_sha256 IS NOT NULL;

ALTER TABLE game_livery ADD COLUMN blob_sha256 bytea;
ALTER TABLE game_livery ADD CONSTRAINT game_livery_blob
  FOREIGN KEY (profile_id, blob_sha256) REFERENCES livery_blob (profile_id, sha256);
ALTER TABLE game_livery ADD CONSTRAINT game_livery_one_copy
  CHECK (data IS NULL OR blob_sha256 IS NULL);
-- Moderation: a hidden livery is answered as missing to everyone but its owner.
ALTER TABLE game_livery ADD COLUMN hidden_at timestamptz;
CREATE INDEX game_livery_blob_ref ON game_livery (profile_id, blob_sha256)
  WHERE blob_sha256 IS NOT NULL;
CREATE INDEX game_livery_car ON game_livery (game, profile_id, car_id, created_at DESC);

-- 3. Moderation: an account a moderator blocked from uploading liveries (every profile of it).
CREATE TABLE livery_upload_block (
  account_id uuid PRIMARY KEY REFERENCES account (id),
  blocked_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- 4. A Hardcore -> Noob change keeps the liveries (owner 2026-10-27 "a mode change keeps the
--    liveries"): they no longer depend on the save line. Erasure still deletes them.
DELETE FROM cloud_purge_target
 WHERE table_name IN ('game_livery'::regclass, 'game_livery_draft'::regclass);

-- The runtime writes its own blobs (the no-op UPDATE of an upsert locks a row a concurrent
-- release would delete) and releases them; moderation (fi-core, the runtime login) hides,
-- deletes and blocks.
GRANT SELECT, INSERT, DELETE ON livery_blob TO fi_runtime;
GRANT UPDATE (size, hidden_at) ON livery_blob TO fi_runtime;
GRANT UPDATE (blob_sha256) ON game_livery_draft TO fi_runtime;
GRANT UPDATE (hidden_at) ON game_livery TO fi_runtime;
GRANT SELECT, INSERT, DELETE ON livery_upload_block TO fi_runtime;
GRANT SELECT, DELETE ON livery_blob, livery_upload_block TO fi_maintenance;
