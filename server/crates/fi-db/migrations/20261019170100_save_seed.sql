-- OD-60 (register CR-S20; SAVES_API.md §4.8, CLOUD_SAVES.md §7): the server's clean Hardcore
-- snapshot. One bundle per game and kind, stored once in the save bucket under its digest (a
-- save_blob row, the save-blob rules of CLOUD_SAVES.md §6), is a Hardcore line's revision 1: the
-- seed route commits it as `fresh_career` naming the same blob, and sets the RPC gates' baseline from
-- the numbers recorded here at import.
--
-- - At most one active seed per game and kind; importing another retires the active one (lines keep
--   the revision they started from). Written by `fi-cli seed-import` as the migrator (an operator's
--   configuration step, like the pools); the runtime only reads.
-- - credits, xp, level: the bundle's summary at import, required (fail closed: the gates' base);
--   winnings 0 (the summary has none; a brand-new career has none); cars as the summary counts the
--   garage, NULL while the reader cannot (the RPC baseline then takes 0).
-- - The blob's foreign key keeps the object: saves-gc must keep every object a seed names.
CREATE TABLE save_seed (
  id             uuid PRIMARY KEY DEFAULT uuidv7(),
  game           text NOT NULL REFERENCES game (code),
  kind           text NOT NULL CHECK (kind IN ('hardcore')),
  blob_sha256    bytea NOT NULL REFERENCES save_blob (sha256),
  content_sha256 bytea NOT NULL CHECK (length(content_sha256) = 32),
  size_bytes     integer NOT NULL CHECK (size_bytes BETWEEN 1 AND 393216),
  -- The Azimuth revision folder number of the bundle (fi_saves label_hex).
  label          text NOT NULL CHECK (label ~ '^[0-9A-F]{16}$'),
  credits        bigint NOT NULL CHECK (credits >= 0),
  xp             bigint NOT NULL CHECK (xp >= 0),
  level          bigint NOT NULL CHECK (level >= 0),
  winnings       bigint NOT NULL DEFAULT 0 CHECK (winnings >= 0),
  cars           integer CHECK (cars >= 0),
  active         boolean NOT NULL DEFAULT true,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  retired_at     timestamptz,
  CONSTRAINT save_seed_active_shape CHECK (active = (retired_at IS NULL))
);

CREATE UNIQUE INDEX save_seed_one_active ON save_seed (game, kind) WHERE active;
CREATE INDEX save_seed_blob ON save_seed (blob_sha256);

GRANT SELECT ON save_seed TO fi_runtime;
GRANT SELECT ON save_seed TO fi_maintenance;
