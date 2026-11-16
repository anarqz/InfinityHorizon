-- OD-147, MATCHMAKING_API.md §3.9 (register CR-S79): peer observations and the cheater pool. In a
-- Hardcore world each player's hook measures the other players' cars and reports what it saw; the
-- server flags a profile on a quorum of the world's other members (fi_db::observations). This
-- migration holds the observations, the quorums reached and the flags. The pool filter that reads
-- `cheat_flag` ships in a later release (§3.9 "Order of delivery"); until then a flag changes nothing.
--
-- Expand only: new tables, read by no older build.

-- One row per observation (the client-minted id is the idempotency key). `counts` is decided when
-- the row is stored: a counting kind (speed, climb, car, parts) whose numbers break the rules it
-- names; `jump` and `clock` never count (calibration first).
CREATE TABLE peer_observation (
  id                  uuid PRIMARY KEY,
  world_id            uuid NOT NULL REFERENCES world (id) ON DELETE CASCADE,
  reporter_profile_id uuid NOT NULL REFERENCES profile (id),
  observed_profile_id uuid NOT NULL REFERENCES profile (id),
  kind                text NOT NULL CHECK (kind IN ('speed', 'climb', 'car', 'parts', 'jump', 'clock')),
  value               double precision CHECK (value IS NULL OR (value > -1e9 AND value < 1e9)),
  sustain_ms          integer CHECK (sustain_ms IS NULL OR sustain_ms >= 0),
  rules_version       integer NOT NULL CHECK (rules_version > 0),
  counts              boolean NOT NULL,
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT peer_observation_not_self CHECK (reporter_profile_id <> observed_profile_id)
);
-- The quorum: distinct reporters of one (world, observed, kind) in the last 10 minutes.
CREATE INDEX peer_observation_quorum ON peer_observation (world_id, observed_profile_id, kind, created_at)
  WHERE counts;
CREATE INDEX peer_observation_at ON peer_observation (created_at);
CREATE INDEX peer_observation_reporter ON peer_observation (reporter_profile_id);
CREATE INDEX peer_observation_observed ON peer_observation (observed_profile_id);

-- A quorum reached about a profile in one world for one kind, with the world's host: a non-host is
-- flagged only on quorums under two different hosts within 24 hours (a host relays every joiner's
-- view and could forge it).
CREATE TABLE cheat_quorum (
  world_id            uuid NOT NULL REFERENCES world (id) ON DELETE CASCADE,
  observed_profile_id uuid NOT NULL REFERENCES profile (id),
  kind                text NOT NULL CHECK (kind IN ('speed', 'climb', 'car', 'parts')),
  host_profile_id     uuid NOT NULL REFERENCES profile (id),
  reached_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (world_id, observed_profile_id, kind)
);
CREATE INDEX cheat_quorum_observed ON cheat_quorum (observed_profile_id, reached_at);
CREATE INDEX cheat_quorum_host ON cheat_quorum (host_profile_id);

-- The cheater pool's members: a profile (one mode: flags come only from Hardcore worlds) until
-- `until`. A new quorum moves `until`, never shortens it.
CREATE TABLE cheat_flag (
  profile_id uuid PRIMARY KEY REFERENCES profile (id),
  until      timestamptz NOT NULL,
  first_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  last_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  kinds      text[] NOT NULL CHECK (kinds <@ ARRAY['speed', 'climb', 'car', 'parts']::text[]),
  CONSTRAINT cheat_flag_order CHECK (first_at <= last_at AND last_at < until)
);
CREATE INDEX cheat_flag_until ON cheat_flag (until);

GRANT SELECT, INSERT ON peer_observation TO fi_runtime;
GRANT SELECT, INSERT ON cheat_quorum TO fi_runtime;
GRANT UPDATE (host_profile_id, reached_at) ON cheat_quorum TO fi_runtime;
GRANT SELECT, INSERT ON cheat_flag TO fi_runtime;
GRANT UPDATE (until, last_at, kinds) ON cheat_flag TO fi_runtime;
-- A player's profile deletion erases its rows (fi_db::erasure, as fi_runtime and fi_maintenance).
GRANT DELETE ON peer_observation, cheat_quorum, cheat_flag TO fi_runtime;
-- Retention (fi_db::retention): observations after 30 days, quorums after a day, flags once ended;
-- `fi-cli cheat-report` reads all three.
GRANT SELECT, DELETE ON peer_observation, cheat_quorum, cheat_flag TO fi_maintenance;
