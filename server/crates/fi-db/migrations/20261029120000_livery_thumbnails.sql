-- The server keeps livery thumbnails (owner decision OD-137, 2026-10-29: "Our server will keep
-- everything. We need liveries thumbnails stored."; GAME_PLATFORM.md §3a "Thumbnails", register
-- CR-S73). Written only through fi_db::liveries.
--
-- One row per versioned livery: the `LiveryThumbnail` its CreateVersionedLivery carried, as the
-- game sent it, with its size and the format sniffed from its first bytes. It goes with its
-- livery (ON DELETE CASCADE): a repaint's RemoveReferenceToVersionedLivery, the per-car and
-- per-profile caps, a moderator's delete and erasure (fi_erase_profile deletes game_livery) take
-- it along. Outside the account's byte quota (FI_LIVERY_ACCOUNT_BYTES). Games are still answered
-- empty thumbnails; serving them outside the game is a later step.
CREATE TABLE game_livery_thumbnail (
  livery_id  bigint PRIMARY KEY REFERENCES game_livery (id) ON DELETE CASCADE,
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  -- VersionedLiveryData.liveryThumbnail's max_len in the game's schema: 400 KiB.
  size       integer NOT NULL CHECK (size BETWEEN 1 AND 409600),
  format     text NOT NULL CHECK (format IN ('png', 'jpeg', 'dds', 'bmp', 'unknown')),
  data       bytea NOT NULL CHECK (octet_length(data) = size),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX game_livery_thumbnail_profile ON game_livery_thumbnail (profile_id);

GRANT SELECT, INSERT, DELETE ON game_livery_thumbnail TO fi_runtime;
GRANT SELECT, DELETE ON game_livery_thumbnail TO fi_maintenance;
