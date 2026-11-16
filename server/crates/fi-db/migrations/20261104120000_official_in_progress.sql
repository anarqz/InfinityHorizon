-- OD-194 (2026-11-04): an official Road Trip takes newcomers only during its destination vote. The
-- agent announces `inProgress` once the trip is past it; the quick join's placement
-- (fi_db::matchmaking::OFFICIAL_PLACEMENT_SQL) skips such a world for anyone not already seated
-- there, so a newcomer gets a lobby still voting or the agent's fresh spare. Every announce sets it
-- (absent: false), so an agent that stops sending it never leaves a world closed.
-- Forward only: applied migrations are immutable.

ALTER TABLE world
  ADD COLUMN in_progress boolean NOT NULL DEFAULT false;
