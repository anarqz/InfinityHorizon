-- Private lobbies on official Worlds (OD-204, CR-S96; WORLDS_OFFICIAL.md §12, MATCHMAKING_API.md
-- §2.10). A private world is an official group world with an owner: never quick-joined or listed,
-- closed once no seat is live. Who may enter it is `world_access`, a game session is linked to it,
-- and a play session declares the client's capability. Forward only: applied migrations are immutable.

ALTER TABLE play_session ADD COLUMN features text[] NOT NULL DEFAULT '{}';

ALTER TABLE world
  ADD COLUMN official_owner_profile_id uuid REFERENCES profile (id) ON DELETE SET NULL,
  ADD COLUMN official_owner_account_id uuid REFERENCES account (id) ON DELETE SET NULL,
  -- Since when one seat at most is live (the lone-owner rule, WORLDS_OFFICIAL.md §12.7).
  ADD COLUMN private_lone_since timestamptz,
  -- The leader the agent reports (§12.8b): a seated member's XUID. NULL: not reported yet, the owner leads.
  ADD COLUMN private_leader_xuid bigint;

ALTER TABLE world
  ADD CONSTRAINT world_official_owner_shape CHECK (
    (official_owner_profile_id IS NULL AND official_owner_account_id IS NULL)
    OR (host_kind = 'official' AND official_group AND activity <> 'carmeet')
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_official_owner_shape;

-- One private world per owner (§12.7).
CREATE UNIQUE INDEX world_one_private_per_owner ON world (official_owner_profile_id)
  WHERE closed_at IS NULL AND official_owner_profile_id IS NOT NULL;

CREATE TABLE world_access (
  world_id uuid NOT NULL REFERENCES world (id) ON DELETE CASCADE,
  account_id uuid NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  via text NOT NULL CHECK (via IN ('owner', 'invite')),
  added_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (world_id, account_id)
);
CREATE INDEX world_access_account ON world_access (account_id);

GRANT SELECT, INSERT, DELETE ON world_access TO fi_runtime;
GRANT SELECT, DELETE ON world_access TO fi_maintenance;

ALTER TABLE freeroam_session
  ADD COLUMN private_world_id uuid REFERENCES world (id) ON DELETE SET NULL;
CREATE INDEX freeroam_session_private_world ON freeroam_session (private_world_id)
  WHERE private_world_id IS NOT NULL;

-- The sweep closes worlds, and a closing private world ends its linked session.
GRANT UPDATE ON freeroam_session TO fi_maintenance;
