-- MATCHMAKING_API.md §5d (OD-159/OD-160, register CR-S80): the reports the relay's anti-cheat
-- detector pushes (`POST /mm/v1/relay/reports`). The relay detects; matchmaking re-judges each report
-- with its own WatchRules of the version named and stores it with an outcome (fi_db::relay_reports).
-- Step 1 of §5d's rollout (rule `relay-reports-record-only` in the code) stores `would_flag` /
-- `would_flag_host` and flags nobody; `flagged*` belong to step 2.
--
-- Rows carry ids and the report's numbers, never a position, an address or a gamertag. The world
-- id has no foreign key (a report about an unknown world is stored `rejected` with reason `world`);
-- the profiles do (erasure deletes every row naming one, fi_db::erasure).
--
-- Expand only: new tables, read by no older build.
CREATE TABLE relay_report (
  id                  uuid PRIMARY KEY,
  episode             uuid NOT NULL,
  relay_key           text NOT NULL CHECK (relay_key ~ '^[A-Za-z0-9+/]{43}=$'),
  start_id            uuid NOT NULL,
  type                text NOT NULL CHECK (type IN ('violation', 'forgery')),
  kind                text NOT NULL CHECK (kind IN ('speed', 'climb', 'jump')),
  world_id            uuid NOT NULL,
  -- violation: the measured profile; forgery: the player whose data was forged.
  measured_profile_id uuid REFERENCES profile (id),
  host_profile_id     uuid REFERENCES profile (id),
  from_at             timestamptz NOT NULL,
  to_at               timestamptz NOT NULL,
  rules_version       integer NOT NULL CHECK (rules_version > 0),
  peak_kmh            double precision NOT NULL CHECK (peak_kmh >= 0 AND peak_kmh <= 10000),
  over_limit_ms       integer NOT NULL CHECK (over_limit_ms >= 0),
  own_peak_kmh        double precision CHECK (own_peak_kmh IS NULL OR (own_peak_kmh >= 0 AND own_peak_kmh <= 10000)),
  legs                integer CHECK (legs IS NULL OR legs >= 0),
  max_jump_m          double precision CHECK (max_jump_m IS NULL OR max_jump_m >= 0),
  outcome             text NOT NULL CHECK (outcome IN ('rejected', 'recorded', 'would_flag', 'would_flag_host', 'flagged', 'flagged_host')),
  reason              text CHECK (reason IN ('world', 'relay', 'member', 'host', 'stale', 'rules', 'transport', 'scope', 'jump', 'dedicated')),
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT relay_report_order CHECK (from_at <= to_at),
  CONSTRAINT relay_report_forgery_kind CHECK (type = 'violation' OR kind <> 'jump')
);
CREATE INDEX relay_report_at ON relay_report (created_at);
CREATE INDEX relay_report_measured ON relay_report (measured_profile_id) WHERE measured_profile_id IS NOT NULL;
CREATE INDEX relay_report_host ON relay_report (host_profile_id) WHERE host_profile_id IS NOT NULL;
CREATE INDEX relay_report_episode ON relay_report (episode);

-- Per relay node key and start: the counters its batches carried, summed (rings_in_use: the
-- latest), for `fi-cli cheat-report`. Ids and numbers only.
CREATE TABLE relay_report_counters (
  relay_key        text NOT NULL CHECK (relay_key ~ '^[A-Za-z0-9+/]{43}=$'),
  start_id         uuid NOT NULL,
  batches          bigint NOT NULL DEFAULT 1,
  decoded          bigint NOT NULL DEFAULT 0,
  undecodable      bigint NOT NULL DEFAULT 0,
  foreign_owner    bigint NOT NULL DEFAULT 0,
  xuid_mismatch    bigint NOT NULL DEFAULT 0,
  witness_full     bigint NOT NULL DEFAULT 0,
  rules_missing    bigint NOT NULL DEFAULT 0,
  rule_unevaluable bigint NOT NULL DEFAULT 0,
  report_dropped   bigint NOT NULL DEFAULT 0,
  rings_in_use     bigint NOT NULL DEFAULT 0,
  first_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (relay_key, start_id)
);
CREATE INDEX relay_report_counters_at ON relay_report_counters (updated_at);

GRANT SELECT, INSERT ON relay_report TO fi_runtime;
GRANT SELECT, INSERT, UPDATE ON relay_report_counters TO fi_runtime;
-- A player's profile deletion erases its rows (fi_db::erasure, as fi_runtime and fi_maintenance).
GRANT DELETE ON relay_report TO fi_runtime;
-- Retention (fi_db::retention): reports and counters after 30 days; `fi-cli cheat-report` reads both.
GRANT SELECT, DELETE ON relay_report, relay_report_counters TO fi_maintenance;
