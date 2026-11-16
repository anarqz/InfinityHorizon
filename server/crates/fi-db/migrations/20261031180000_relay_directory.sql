-- Relay by mode (OD-160; MATCHMAKING_API.md §5b "Relay by mode"): what the server last read of the
-- beholder's relay list (ANNOUNCER_API §3), per trusted relay. A Hardcore world opens on its relay
-- unless a check within 90 s did not list it (fi_db::matchmaking::relays_down_in); no row, or an
-- older check, says nothing. Written by every matchmaking instance's 30-second poll
-- (fi_db::matchmaking::record_relay_directory); the latest check wins.
--
-- Expand only: a new table the previous build never reads. No player data: relay public keys only.

CREATE TABLE mm_relay_directory (
  relay_key  text PRIMARY KEY CHECK (char_length(relay_key) BETWEEN 1 AND 64),
  listed     boolean NOT NULL,
  checked_at timestamptz NOT NULL
);

GRANT SELECT, INSERT, UPDATE ON mm_relay_directory TO fi_runtime;
