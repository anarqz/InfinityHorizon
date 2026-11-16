-- SER-M3 step 6, the scoreboard lane (OD-48; docs/contracts/GAME_PLATFORM.md §3a "Supporter
-- universe", register CR-S18): the game's own leaderboards, ghosts and XP ledger, written by the
-- Forza RPC (`Scoreboard.SubmitScore`, `UploadFile`, `SubmitXpScore`) through fi_db::scores, each in
-- the request's one transaction with its progression fact. Ported from the Beta's SQLite `scores`,
-- `ghosts` and `xp` (deprecated/beta/server/src/db.rs), keyed by profile and mode instead of XUID.
--
-- A board is the game's key: (scoreboardType, scoreType, gameScoreboardId, trackId). Score type 1 is
-- a time (lower is better), every other a speed or points (higher is better): fi_db::scores
-- ::lower_is_better. Every per-game table carries `game` as the first column of its keys and
-- indexes (ADR 0902 §1). Nothing of the Beta is imported (OD-48). `board_entry` and `ghost`
-- (20261019000300_universe.sql, POST /saves/v1/progress) are not the game's boards.
--
-- Readers apply fi_db::visibility::VISIBLE (same game and mode, the row's mode its profile's,
-- profile and account active, account not banned). The runtime role never deletes; erasure and
-- retention are fi_maintenance's.

-- 1. Each profile's best per event: one row per profile and board key, replaced only by a strictly
-- better score (an equal one keeps the earlier). `score_id` is the wire's scoreId: the id of the
-- SubmitScore fact that set the row (progression_event.event_id), so a replayed request answers the
-- same id and UploadFile names the row it belongs to.
CREATE TABLE game_score (
  game               text NOT NULL REFERENCES game (code),
  mode               text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  profile_id         uuid NOT NULL REFERENCES profile (id),
  scoreboard_type    integer NOT NULL,
  score_type         integer NOT NULL,
  game_scoreboard_id integer NOT NULL,
  -- i16 on the wire; -1 for boards without a track (speed traps and zones, skills).
  track_id           integer NOT NULL CHECK (track_id BETWEEN -32768 AND 32767),
  -- Finite, positive, at most 1e9 (NaN and the infinities fail the range in Postgres too).
  score              double precision NOT NULL CHECK (score > 0 AND score <= 1e9),
  score_id           uuid NOT NULL,
  car_id             integer NOT NULL CHECK (car_id BETWEEN -32768 AND 32767),
  car_class_id       smallint NOT NULL CHECK (car_class_id BETWEEN 0 AND 255),
  -- carPerformanceIndex as submitted (0.608 for PI 608).
  car_pi             real NOT NULL,
  is_clean           boolean NOT NULL,
  used_stm           boolean NOT NULL,
  used_tcs           boolean NOT NULL,
  used_abs           boolean NOT NULL,
  used_auto_brake    boolean NOT NULL,
  used_auto_shifting boolean NOT NULL,
  used_clutch        boolean NOT NULL,
  -- The submitted ScoreboardScoreData (a few hundred bytes).
  data               jsonb NOT NULL CHECK (jsonb_typeof(data) = 'object' AND pg_column_size(data) <= 4096),
  submitted_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, mode, profile_id, scoreboard_type, score_type, game_scoreboard_id, track_id)
);

-- A route board (any event on a track) and an event board (trackless boards name their event).
CREATE INDEX game_score_track ON game_score (game, mode, scoreboard_type, score_type, track_id, score);
CREATE INDEX game_score_event ON game_score (game, mode, scoreboard_type, score_type, game_scoreboard_id, score);
-- UploadFile and the replay answer find a row by its scoreId.
CREATE UNIQUE INDEX game_score_id ON game_score (game, score_id);

-- 2. Ghosts: one per profile and board key, the recording of the score that was the profile's best
-- when it was uploaded. `file_id` is the wire's integer ghostFileId (i32: the identity stops at its
-- maximum); replacing a ghost keeps it, as the Beta's did. The recording is a create-only object in
-- the cloud-saves bucket under its SHA-256 (ghost_blob, CLOUD_SAVES.md §6), put before the
-- transaction that writes this row.
CREATE TABLE game_ghost (
  game               text NOT NULL REFERENCES game (code),
  mode               text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  profile_id         uuid NOT NULL REFERENCES profile (id),
  scoreboard_type    integer NOT NULL,
  score_type         integer NOT NULL,
  game_scoreboard_id integer NOT NULL,
  track_id           integer NOT NULL CHECK (track_id BETWEEN -32768 AND 32767),
  file_id            bigint GENERATED ALWAYS AS IDENTITY (MAXVALUE 2147483647) NOT NULL,
  score_id           uuid NOT NULL,
  score              double precision NOT NULL CHECK (score > 0 AND score <= 1e9),
  car_id             integer NOT NULL CHECK (car_id BETWEEN -32768 AND 32767),
  blob_sha256        bytea NOT NULL REFERENCES ghost_blob (sha256),
  size               integer NOT NULL CHECK (size BETWEEN 1 AND 256000),
  -- The UploadFile fact that wrote the row.
  event_id           uuid NOT NULL,
  recorded_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, mode, profile_id, scoreboard_type, score_type, game_scoreboard_id, track_id)
);

-- DownloadFile by id; a rival's nearest ghost on a route.
CREATE UNIQUE INDEX game_ghost_file ON game_ghost (game, file_id);
CREATE INDEX game_ghost_route ON game_ghost (game, mode, scoreboard_type, score_type, track_id, score);
CREATE INDEX game_ghost_blob ON game_ghost (blob_sha256);

-- 3. The XP ledger (Scoreboard.SubmitXpScore): one row per accepted report fact, clamped to the
-- most one report carries; the XP-rate gate sums a profile's last hour.
CREATE TABLE game_xp (
  event_id   uuid PRIMARY KEY,
  game       text NOT NULL REFERENCES game (code),
  mode       text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  profile_id uuid NOT NULL REFERENCES profile (id),
  xp         double precision NOT NULL CHECK (xp >= 0 AND xp <= 50000),
  at         timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX game_xp_recent ON game_xp (game, profile_id, at) INCLUDE (xp);

GRANT SELECT, INSERT, UPDATE ON game_score, game_ghost TO fi_runtime;
GRANT SELECT, INSERT ON game_xp TO fi_runtime;
GRANT SELECT, DELETE ON game_score, game_ghost, game_xp TO fi_maintenance;
