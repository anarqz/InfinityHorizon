-- OD-210 (CR-S101, MATCHMAKING_API.md §2.11): official lobbies are mixed. A public official world
-- may admit both modes whatever its activity (`any_mode`; a car meet still always does), and a
-- private official world's current leader picks who may join from then on: `join_mode` NULL is
-- Everyone, else the one mode new joins, invites and placements admit. The leader's choice never
-- touches `mode` or `any_mode`, so seats already granted (and reserved) stay valid whatever their
-- mode: §3.2's per-mode seat defence reads `any_mode`, the admission of new seats reads
-- `join_mode` too. Only a private world gets a
-- `join_mode` (the route checks the owner; the shape only an official group world, so an owner's
-- erasure, which sets the owner NULL, never trips it). Forward only: applied migrations are immutable.

ALTER TABLE world DROP CONSTRAINT world_host_kind_shape;
ALTER TABLE world
  ADD CONSTRAINT world_host_kind_shape CHECK (
    (host_kind = 'player' AND host_profile_id IS NOT NULL AND host_account_id IS NOT NULL AND host_vm_id IS NULL
        AND NOT official_group)
    OR (host_kind = 'official' AND host_profile_id IS NULL AND host_account_id IS NULL AND host_session_id IS NULL
        AND host_vm_id ~ '^[a-z0-9-]{1,32}$' AND NOT dedicated AND transport = 'direct' AND relay_key IS NULL
        AND NOT pool AND audience = 'everyone' AND kind = 'free' AND host_tier = 'free' AND mods_key IS NULL
        AND allow IS NULL
        AND (activity <> 'carmeet' OR (any_mode AND NOT official_group)))
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_host_kind_shape;

ALTER TABLE world ADD COLUMN join_mode text;
ALTER TABLE world
  ADD CONSTRAINT world_join_mode_shape CHECK (
    join_mode IS NULL
    OR (join_mode IN ('hardcore', 'noob') AND any_mode AND host_kind = 'official' AND official_group)
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_join_mode_shape;
