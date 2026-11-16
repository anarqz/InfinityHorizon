-- SER-M3 step 5 (docs/contracts/MATCHMAKING_API.md §4a; docs/server/SER-M3_PORT_MAP.md §3): the
-- game's free-roam hosts (sessiondirectory, momatch) on the game plane. The Beta kept MPSD
-- sessions and SmartMatch tickets in one process's memory; here they are rows, so any instance
-- answers any request (services/AGENTS.md: no cross-request state in a process). Writers:
-- fi_db::freeroam (the game-plane library) and the announce of fi_db::matchmaking (the LAN list).
--
-- Expiry is a time filter on every read (`expires_at > clock_timestamp()`); "deleting" a session
-- or a ticket sets its `expires_at` to now, since fi_runtime holds no DELETE. `fi-cli
-- freeroam-purge` (fi_maintenance) removes rows expired for over an hour.

-- 1. The host's private addresses (the announce's `endpoint.lan`): a joiner on the host's public
-- address, or any joiner of a host seen on loopback, is sent to the first one (§4a).
ALTER TABLE world ADD COLUMN endpoint_lan inet[] NOT NULL DEFAULT '{}'
  CONSTRAINT world_endpoint_lan_max CHECK (cardinality(endpoint_lan) <= 4);

-- 2. MPSD sessions (sessiondirectory, contract 105), keyed by the game's path, lower case. Only
-- the owner (the profile that created a live session) reads, writes or deletes it; for anybody
-- else it does not exist. `revision` is the ETag; a row past `expires_at` is a session that does
-- not exist (a write by anyone recreates it from scratch). At most 16 live per owner.
CREATE TABLE freeroam_session (
  game             text NOT NULL REFERENCES game (code),
  scid             text NOT NULL CHECK (scid ~ '^[0-9a-z._-]{1,128}$'),
  template         text NOT NULL CHECK (template ~ '^[0-9a-z._-]{1,128}$'),
  name             text NOT NULL CHECK (name ~ '^[0-9a-z._-]{1,128}$'),
  owner_profile_id uuid NOT NULL REFERENCES profile (id),
  -- The document as the game reads it (≤ 128 KiB serialized, checked by the writer; the jsonb
  -- text form is longer, hence the looser backstop).
  doc              jsonb NOT NULL CHECK (jsonb_typeof(doc) = 'object' AND octet_length(doc::text) <= 262144),
  revision         bigint NOT NULL CHECK (revision >= 0),
  created_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at       timestamptz NOT NULL,
  PRIMARY KEY (game, scid, template, name)
);

CREATE INDEX freeroam_session_owner ON freeroam_session (game, owner_profile_id, updated_at);
CREATE INDEX freeroam_session_name ON freeroam_session (game, owner_profile_id, name);
CREATE INDEX freeroam_session_expiry ON freeroam_session (expires_at);

-- 3. SmartMatch tickets (momatch, contract 103): every FreeRoam_qos ticket is found at once; the
-- row keeps what GET .../tickets/{id} answers, the world it was sent to and how that world was
-- chosen (`via`: hosting, grant, auto, fallback). At most 16 live per owner.
CREATE TABLE freeroam_ticket (
  id               uuid PRIMARY KEY,
  game             text NOT NULL REFERENCES game (code),
  owner_profile_id uuid NOT NULL REFERENCES profile (id),
  hopper           text NOT NULL CHECK (hopper ~ '^[0-9a-z._-]{1,128}$'),
  details          jsonb NOT NULL CHECK (jsonb_typeof(details) = 'object' AND octet_length(details::text) <= 262144),
  world_id         uuid REFERENCES world (id),
  via              text NOT NULL CHECK (via IN ('hosting', 'grant', 'auto', 'fallback')),
  created_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at       timestamptz NOT NULL,
  CONSTRAINT freeroam_ticket_world_shape CHECK ((world_id IS NULL) = (via = 'fallback'))
);

CREATE INDEX freeroam_ticket_owner ON freeroam_ticket (game, owner_profile_id, created_at);
CREATE INDEX freeroam_ticket_hopper ON freeroam_ticket (game, hopper, expires_at);
CREATE INDEX freeroam_ticket_expiry ON freeroam_ticket (expires_at);

GRANT SELECT, INSERT, UPDATE ON freeroam_session, freeroam_ticket TO fi_runtime;
GRANT SELECT, DELETE ON freeroam_session, freeroam_ticket TO fi_maintenance;
