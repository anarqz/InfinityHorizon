-- Reports from the game (2026-10-19; OD-63; docs/contracts/CORE_API.md §4.3, register CR-L25).
-- The overlay reports a player of the world's roster through the launcher (no Turnstile: having
-- shared a world in the last 30 minutes stands in for it). Written by fi-core through
-- fi_db::moderation::game_report_in, into the social reports' table, counted by the three-report
-- rule unchanged (ADR 0204).
--
-- 1. report: where a report comes from (`web`: POST /core/v1/social/reports; `game`: POST
--    /launcher/v1/reports) and, for the game's, the world the two shared when the overlay named
--    it. A report from the game names a player and carries no free text (v1). The world is
--    context, not evidence: a retention job that deletes a closed world keeps the report.
ALTER TABLE report
  ADD COLUMN source text NOT NULL DEFAULT 'web' CONSTRAINT report_source_check CHECK (source IN ('web', 'game')),
  ADD COLUMN world_id uuid REFERENCES world (id) ON DELETE SET NULL,
  ADD CONSTRAINT report_world_from_game CHECK (world_id IS NULL OR source = 'game'),
  ADD CONSTRAINT report_game_shape CHECK (source = 'web' OR (target_kind = 'player' AND text IS NULL));

-- One report per reporter account and target a day: the reporter's newest report on a target.
CREATE INDEX report_reporter_target ON report (reporter_account_id, target_kind, target_id, created_at DESC);

-- 2. Who shared a world in the last 30 minutes, read from the target profile: the worlds it
--    hosted that closed since (its open one is world_one_open_per_host; its seats are
--    mm_grant_one_live and mm_grant_profile_ended), then the reporter account's seats in those
--    worlds, any state.
CREATE INDEX world_host_closed ON world (game, host_profile_id, closed_at DESC) WHERE closed_at IS NOT NULL;
CREATE INDEX mm_grant_world_account ON mm_grant (world_id, account_id);
