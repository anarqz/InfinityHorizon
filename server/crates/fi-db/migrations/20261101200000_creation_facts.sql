-- OD-178 (GAME_PLATFORM.md §3a "Supporter universe", RPC gates; EVENTS.md §3.3): a gated
-- (Hardcore) creation for a car the game cannot have or a traffic car is refused and recorded as
-- a `creation` fact (the request's fact id, so a replayed request writes nothing). Expand only:
-- the kind CHECK gains `creation`; the previous build never writes it.
ALTER TABLE progression_event DROP CONSTRAINT progression_event_kind_check;
ALTER TABLE progression_event ADD CONSTRAINT progression_event_kind_check
  CHECK (kind IN ('profile_sync', 'tables_sync', 'score', 'xp', 'race_result', 'drivatar', 'livery', 'ghost', 'club', 'creation'))
  NOT VALID;
ALTER TABLE progression_event VALIDATE CONSTRAINT progression_event_kind_check;
