-- OD-150 (MATCHMAKING_API.md §2.5): what a world runs now, set by its host's launcher and changed in
-- real time; players join by it.
ALTER TABLE world ADD COLUMN activity text NOT NULL DEFAULT 'freeroam'
  CONSTRAINT world_activity_known CHECK (activity IN ('freeroam', 'roadtrip'));
