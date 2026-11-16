-- CR-S51 (SAVES_API.md §4.9, CLOUD_SAVES.md §7.4; owner-decisions OD-122 follow-ups 2026-10-26):
-- a Hardcore line may start from Azimuth's own first save, committed by the player's launcher as
-- kind `fresh_start` (revision 1, actor `player`) and verified by the server's fresh-start check
-- (fi_saves::fresh). Expand only: the kind CHECK and the kind/actor CHECK gain `fresh_start`;
-- every existing row satisfies both (no row has the new kind), so a rolling deploy's older
-- instances, which never write it, are unaffected. fi_saves CommitKind::ALL and
-- CommitKind::allows say the same.
ALTER TABLE save_revision DROP CONSTRAINT save_revision_kind_check;
ALTER TABLE save_revision ADD CONSTRAINT save_revision_kind_check CHECK (kind IN (
  'upload', 'resolve_keep_local', 'restore', 'fresh_career', 'import_offline', 'beta_import',
  'moderation_reset', 'season_reset', 'career_reset', 'parked_accept', 'fresh_start'));

ALTER TABLE save_revision DROP CONSTRAINT save_revision_kind_actor;
ALTER TABLE save_revision ADD CONSTRAINT save_revision_kind_actor CHECK (
  CASE kind
    WHEN 'upload' THEN actor = 'player'
    WHEN 'resolve_keep_local' THEN actor = 'player'
    WHEN 'import_offline' THEN actor = 'player'
    WHEN 'fresh_start' THEN actor = 'player'
    WHEN 'restore' THEN actor IN ('player', 'moderator')
    WHEN 'fresh_career' THEN actor = 'system'
    WHEN 'beta_import' THEN actor = 'import'
    WHEN 'moderation_reset' THEN actor = 'moderator'
    WHEN 'parked_accept' THEN actor = 'moderator'
    ELSE actor IN ('system', 'moderator')
  END
);
