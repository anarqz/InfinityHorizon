-- Relay by mode, after the fresh-context review (OD-160; MATCHMAKING_API.md §5b "Relay by mode"):
--
-- 1. `mm_relay_directory.missed_since`: since when a trusted relay has been missing from every
--    check (NULL while listed). A relay is down only after 30 s of consecutive misses, so one empty
--    or partial beholder listing (a restart, a stale cache) never opens a Hardcore world direct for
--    its life.
-- 2. `world_invite.relays`: the asker's trusted relay round trips (`RequestToJoin.relays`), so a
--    request to join a relay-born world can be accepted onto that world's relay.
--
-- Expand only: nullable columns the previous build never reads.

ALTER TABLE mm_relay_directory ADD COLUMN missed_since timestamptz;
ALTER TABLE world_invite ADD COLUMN relays jsonb
  CONSTRAINT world_invite_relays_object CHECK (relays IS NULL OR jsonb_typeof(relays) = 'object');
