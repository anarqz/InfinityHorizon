-- OD-121 follow-up "enforcement on" (GATE_API.md §4a): an idle session someone could replace is
-- warned first (`evict_in` on its answers for `warn_secs`), then ended `idle`. `warned_at` is the
-- start of the episode's warning; a session that plays again or that nobody waits for any more has
-- it cleared, so a later idle stretch is warned afresh. Expand only: NULL for every existing row,
-- and the previous build never reads it.
ALTER TABLE session_idle_observation ADD COLUMN warned_at timestamptz;

GRANT UPDATE (warned_at, evicted) ON session_idle_observation TO fi_runtime;
