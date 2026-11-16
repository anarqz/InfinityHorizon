-- Parity with other projects used as reference (2026-10-30, SAURON V6: a Road Trip ticket was sent to a world whose host had no
-- Road Trip engine and the game hung on JOINING SESSION): a world records the lobby engines its
-- host's world server runs (`WorldSettings.lobbies`, MATCHMAKING_API.md §2.5), so the launcher
-- sends a car meet or Road Trip only to a world that runs it. Free roam is every world's; an
-- older launcher announces none. Written when the world opens.
ALTER TABLE world ADD COLUMN lobbies text[] NOT NULL DEFAULT '{}'
  CONSTRAINT world_lobbies_known CHECK (lobbies <@ ARRAY['carmeet', 'roadtrip']::text[]);
