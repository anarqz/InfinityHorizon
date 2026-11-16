-- Lifting a cheat flag (MATCHMAKING_API.md §3.9 "Lifting a flag", CORE_API.md §5
-- `DELETE /core/v1/admin/cheat-flags/{profileId}`, `fi-cli cheat-unflag`; register CR-S79): staff
-- end a flag now (`until` = the lift) and mark it `lifted`, so the next flag of that profile does
-- not double on it (OD-149's repeat counter skips a lifted flag).
--
-- Expand only: a column with a default the previous build never reads; only this build writes it.
ALTER TABLE cheat_flag ADD COLUMN lifted boolean NOT NULL DEFAULT false;

-- The moderator route runs as fi_runtime; the operator's `fi-cli cheat-unflag` as a fi_maintenance
-- login (fi_jobs), which already holds INSERT on admin_action for its operator row.
GRANT UPDATE (lifted) ON cheat_flag TO fi_runtime;
GRANT UPDATE (until, lifted) ON cheat_flag TO fi_maintenance;
