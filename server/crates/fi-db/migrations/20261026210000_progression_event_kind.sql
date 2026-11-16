-- No outbox, no queue (owner-decisions, OD-122 follow-ups 2026-10-26; register CR-S55; ADR 0007
-- superseded): `profile_progress` is recounted in the writer's transaction
-- (`fi_db::progression::refresh_profile`, on every accepted `race_result` fact and every session
-- end), which counts one profile's facts of one kind. This index serves that lookup.
--
-- `outbox_event` and `processed_event` stay in place, unused by this release: a server still
-- running an older image writes them, so a later release drops them once every running image is
-- this one (CR-S55). Expand only.
CREATE INDEX progression_event_profile_kind ON progression_event (profile_id, kind);
