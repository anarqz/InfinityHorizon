-- OD-52 (register CR-S20; SAVES_API.md §4.7): only a Supporter's Hardcore profile keeps a save
-- line, and on renewal the cloud save as of the lapse wins. A line remembers the Supporter coverage
-- it last saw and whether it lapsed since (fi_db::cloud):
--
-- - covered_until, covered_at: the account's entitlement access_until, and the database clock when
--   it was read, at the line's latest lease grant or renewal (an eligible session's admission or
--   heartbeat). Never NULL alone.
-- - lapsed_at: set when a grant finds covered_until already past (the coverage the line last saw
--   ended before this admission), or when the server meets the Hardcore profile while its account
--   is not a Supporter (an admission without a lease, a heartbeat that releases the lease, a
--   refused commit): the instant the known coverage ended, at most the observation's. Reported as
--   SaveHead.lapsedSince while the line has a head; cleared by the line's next player commit.
--
-- fi_runtime already holds SELECT, INSERT, UPDATE on save_head; fi_save_head_guard does not look at
-- these columns.
ALTER TABLE save_head
  ADD COLUMN covered_until timestamptz,
  ADD COLUMN covered_at timestamptz,
  ADD COLUMN lapsed_at timestamptz,
  ADD CONSTRAINT save_head_covered_shape CHECK ((covered_until IS NULL) = (covered_at IS NULL));
