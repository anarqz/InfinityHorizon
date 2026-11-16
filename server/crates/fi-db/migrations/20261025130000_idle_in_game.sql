-- OD-121, GATE_API.md §4a: idle in game. The hook's activity report (`X-Fi-Activity` on the
-- game's 30 s request) is judged by `fi_domain::activity`; the session keeps the track it needs
-- between reports, and every episode of an idle session that someone could replace is recorded.
-- Observation first: nothing ends a session as `idle` until the owner turns `FI_IDLE_EVICT` on.
--
-- Expand only: the new columns are NULL for sessions without a report (`no_signal`, never idle)
-- and every instance of the previous build ignores them.
ALTER TABLE play_session
  ADD COLUMN activity_seq         bigint CHECK (activity_seq >= 0),
  ADD COLUMN activity_odo_base_m  bigint CHECK (activity_odo_base_m >= 0),
  ADD COLUMN activity_progress_at timestamptz,
  ADD COLUMN activity_engaged_at  timestamptz,
  ADD COLUMN activity_reported_at timestamptz,
  ADD COLUMN activity_rp          smallint CHECK (activity_rp BETWEEN 0 AND 255),
  ADD CONSTRAINT play_session_activity_shape CHECK (
    num_nulls(activity_seq, activity_odo_base_m, activity_progress_at, activity_engaged_at,
              activity_reported_at, activity_rp) IN (0, 6));

-- `idle` joins the end reasons (used only while eviction is enforced).
ALTER TABLE play_session DROP CONSTRAINT play_session_end_reason_check;
ALTER TABLE play_session ADD CONSTRAINT play_session_end_reason_check CHECK (end_reason IN
  ('game_exit', 'player', 'takeover', 'expired', 'revoked', 'queue_left', 'max_age', 'idle'));

-- One row per idle episode (a session idle past its window while someone could take its slot),
-- the owner's calibration data (`fi-cli idle-report`). An episode is keyed by the progress it
-- started after, so a session counts once per episode however many reports it sends meanwhile.
CREATE TABLE session_idle_observation (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  game          text NOT NULL REFERENCES game (code),
  session_id    uuid NOT NULL,
  tier          text NOT NULL CHECK (tier IN ('free', 'vip')),
  progress_at   timestamptz NOT NULL,
  idle_secs     integer NOT NULL CHECK (idle_secs >= 0),
  menu_secs     integer NOT NULL CHECK (menu_secs >= 0),
  waiting       integer NOT NULL CHECK (waiting >= 0),
  evicted       boolean NOT NULL DEFAULT false,
  observed_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- The episode's latest look: idle/menu/waiting keep their largest values until it ends.
  last_observed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT session_idle_observation_episode UNIQUE (session_id, progress_at)
);
CREATE INDEX session_idle_observation_game ON session_idle_observation (game, observed_at);

GRANT SELECT, INSERT ON session_idle_observation TO fi_runtime;
GRANT UPDATE (idle_secs, menu_secs, waiting, last_observed_at) ON session_idle_observation TO fi_runtime;
-- Retention deletes old episodes with the rest of fi_db::retention (90 days).
GRANT SELECT, DELETE ON session_idle_observation TO fi_maintenance;
