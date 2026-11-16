-- Official pool worlds (OD-149 after OD-208, CR-S103; WORLDS_OFFICIAL.md §14) and the per-VM safety
-- ceiling's alert (OD-209, OD-218, CR-S100; WORLDS_OFFICIAL.md §13). Forward only: applied
-- migrations are immutable.
--
-- 1. An official world may be a pool world (`pool`, set by the agent's `PUT` and fixed for the
--    world's life): only sessions under a live cheat flag are placed, invited or seated there, and a
--    clean session never. The shape is `20261107200000_mixed_official_worlds.sql`'s without
--    `NOT pool` on the official branch. Expand only: the previous build never opens one.
ALTER TABLE world DROP CONSTRAINT world_host_kind_shape;
ALTER TABLE world
  ADD CONSTRAINT world_host_kind_shape CHECK (
    (host_kind = 'player' AND host_profile_id IS NOT NULL AND host_account_id IS NOT NULL AND host_vm_id IS NULL
        AND NOT official_group)
    OR (host_kind = 'official' AND host_profile_id IS NULL AND host_account_id IS NULL AND host_session_id IS NULL
        AND host_vm_id ~ '^[a-z0-9-]{1,32}$' AND NOT dedicated AND transport = 'direct' AND relay_key IS NULL
        AND audience = 'everyone' AND kind = 'free' AND host_tier = 'free' AND mods_key IS NULL
        AND allow IS NULL
        AND (activity <> 'carmeet' OR (any_mode AND NOT official_group)))
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_host_kind_shape;

-- 2. Each Worlds VM's `FI_WORLDS` `maxWorlds`, the safety ceiling, as the matchmaking service last
--    saw it on that VM's `PUT`, and whether the owner was alerted that the VM's open official
--    worlds passed 80% of it (`fi-cli worlds-ceiling`: one mail when it passes, one when it is back
--    under). No player data.
CREATE TABLE official_vm (
  vm_id         text PRIMARY KEY CHECK (vm_id ~ '^[a-z0-9-]{1,32}$'),
  max_worlds    integer NOT NULL CHECK (max_worlds >= 1),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- Set when the alert mail went out, cleared when the recovery mail went out.
  alerted_at    timestamptz,
  -- The open worlds counted when it was last alerted (or recovered), for the mail.
  alerted_open  integer
);

GRANT SELECT, INSERT, UPDATE (max_worlds, updated_at) ON official_vm TO fi_runtime;
GRANT SELECT, UPDATE (alerted_at, alerted_open) ON official_vm TO fi_maintenance;
