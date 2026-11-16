-- OD-129 (2026-10-28): everything is free for everyone; Supporter is a badge. Any host may open a
-- world of any audience, so a world's audience no longer follows its host's tier (`kind` stays:
-- it shows whether the host is a Supporter).
ALTER TABLE world DROP CONSTRAINT world_audience_follows_tier;
