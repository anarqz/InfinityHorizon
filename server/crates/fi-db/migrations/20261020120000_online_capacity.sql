-- OD-71 (GATE_API.md §2 "Online capacity"): one cap per game across its pools, the most active
-- play sessions of the game at once (NULL unlimited). The gate admits a launch only while the game
-- is under it, Supporters first; the existing queue holds the rest. NULL (unlimited) until set:
-- the owner's starting value (fh2 50) is an operational step, `fi-cli online-capacity --game fh2
-- --set 50` (infra/terraform/README.md "First apply", the local stack), never a migration's, so
-- the test databases' accumulated sessions are not capped and a deploy never overwrites what an
-- admin dialed in (`PUT /core/v1/admin/capacity`).
--
-- Expand only. Instances of the previous build ignore the column until a rolling deploy replaces
-- them (they may admit past the cap for that minute).
ALTER TABLE game
  ADD COLUMN online_capacity integer CHECK (online_capacity BETWEEN 1 AND 100000);

-- Core's admin route and the jobs login (fi_runtime) set it, as they set the maintenance window.
GRANT UPDATE (online_capacity) ON game TO fi_runtime;
