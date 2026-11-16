-- CR-S52 (SAVES_API.md §4.10, CLOUD_SAVES.md §7.6, CORE_API.md §5; owner-decisions 2026-10-26 "a
-- Hardcore profile seeded from an existing profile, checked on the spot"): a Hardcore line may start
-- from another of the account's profiles' save, checked on the spot. Expand only: every existing
-- row satisfies every new or widened CHECK, and an older instance of a rolling deploy never writes
-- the new kind, source or columns (and may lift a ban without clearing `ban_review`: tolerated,
-- §3). Locks: the CHECKs added to save_revision, integrity_assessment and account are validated
-- under ACCESS EXCLUSIVE with a scan of each table (seconds at today's sizes; REVIEW_LOG CR-S52).
--
-- 1. save_revision: kind `seed_from_profile` (actor `player`, revision 1 of a Hardcore line) and
--    its provenance `seed_source`, the profile whose save it is (set exactly on that kind; the
--    bundle digest is the row's own `blob_sha256`). Erasure deletes the account's revisions, so the
--    provenance goes with them (both profiles are the same account's). One source seeds at most
--    one live (not retired) line: checked by fi_db::saves under an advisory lock on the source.
ALTER TABLE save_revision DROP CONSTRAINT save_revision_kind_check;
ALTER TABLE save_revision ADD CONSTRAINT save_revision_kind_check CHECK (kind IN (
  'upload', 'resolve_keep_local', 'restore', 'fresh_career', 'import_offline', 'beta_import',
  'moderation_reset', 'season_reset', 'career_reset', 'parked_accept', 'fresh_start',
  'seed_from_profile'));

ALTER TABLE save_revision DROP CONSTRAINT save_revision_kind_actor;
ALTER TABLE save_revision ADD CONSTRAINT save_revision_kind_actor CHECK (
  CASE kind
    WHEN 'upload' THEN actor = 'player'
    WHEN 'resolve_keep_local' THEN actor = 'player'
    WHEN 'import_offline' THEN actor = 'player'
    WHEN 'fresh_start' THEN actor = 'player'
    WHEN 'seed_from_profile' THEN actor = 'player' AND revision = 1
    WHEN 'restore' THEN actor IN ('player', 'moderator')
    WHEN 'fresh_career' THEN actor = 'system'
    WHEN 'beta_import' THEN actor = 'import'
    WHEN 'moderation_reset' THEN actor = 'moderator'
    WHEN 'parked_accept' THEN actor = 'moderator'
    ELSE actor IN ('system', 'moderator')
  END
);

ALTER TABLE save_revision
  ADD COLUMN seed_source uuid REFERENCES profile (id),
  ADD CONSTRAINT save_revision_seed_source
    CHECK ((kind = 'seed_from_profile') = (seed_source IS NOT NULL)),
  ADD CONSTRAINT save_revision_seed_not_self CHECK (seed_source IS DISTINCT FROM profile_id);

-- The moderation lookup: the lines seeded from a profile (when that source is found altered).
CREATE INDEX save_revision_seed_source ON save_revision (seed_source) WHERE seed_source IS NOT NULL;

-- 2. integrity_assessment: source `seed_check`, every seed check (the dry run `POST .../save/check`
--    and the authoritative one of a `seed_from_profile` commit), accepted or refused, recorded by
--    the bundle's SHA-256 (`bundle_sha256`, never its bytes) on the SOURCE profile (`profile_id`,
--    `mode` its mode; the save checked is its), with the Hardcore profile it would seed
--    (`seed_target`). A dry run has no play session: `play_session` is the nil UUID. A Hardcore ->
--    Noob change purges the assessments of the profile changing (cloud_purge_target): the checks
--    recorded on a source survive its target's change, not the source's own (a Hardcore source
--    turning Noob).
ALTER TABLE integrity_assessment DROP CONSTRAINT integrity_assessment_source_check;
ALTER TABLE integrity_assessment
  ADD CONSTRAINT integrity_assessment_source_check
    CHECK (source IN ('save_commit', 'profile_sync', 'tables_sync', 'save_audit', 'save_nightly',
                      'seed_check')),
  ADD COLUMN bundle_sha256 bytea CHECK (octet_length(bundle_sha256) = 32),
  ADD COLUMN seed_target uuid REFERENCES profile (id),
  ADD CONSTRAINT integrity_assessment_seed_shape
    CHECK ((source = 'seed_check') = (bundle_sha256 IS NOT NULL)
       AND (source = 'seed_check') = (seed_target IS NOT NULL));

-- 3. account.ban_review: why a ban is a block pending review the player is told about
--    (`ACCOUNTS.BANNED {review}`); only `seed_from_profile` (the seed-revoke moderation action).
--    NULL for an ordinary ban; cleared with the ban by this build. No CHECK ties it to
--    `banned_at`: an older instance's lift leaves it set, and every read takes it only while a ban
--    is in force (fi_db::accounts ACCOUNT_COLUMNS, tokens, identity, save retention).
ALTER TABLE account
  ADD COLUMN ban_review text CHECK (ban_review IN ('seed_from_profile'));
