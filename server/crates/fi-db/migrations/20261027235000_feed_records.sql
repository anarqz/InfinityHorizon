-- Discord phase 2: speed and route records in the public feed (owner-decisions 2026-10-27 "Discord
-- phase 2"; CORE_API.md §5a "Records", register CR-S64). `Scoreboard.SubmitScore` appends a
-- `speed_record` or `route_record` row on its own transaction when the score takes the board's #1
-- from another player. The previous holder is the row's actor (account, profile, the gamertag only
-- while they announce), so erasure already removes them (fi_db::erasure, discord::erase_profile_in).
--
-- Expand only: a NULL column and a wider kind CHECK; the previous build writes only its four kinds
-- (record NULL) and its bot ignores kinds it does not know.

ALTER TABLE public_event DROP CONSTRAINT public_event_kind_check;
ALTER TABLE public_event
  ADD CONSTRAINT public_event_kind_check CHECK (kind IN ('player_joined', 'lobby_opened',
    'player_kicked', 'player_banned', 'speed_record', 'route_record')) NOT VALID;
ALTER TABLE public_event VALIDATE CONSTRAINT public_event_kind_check;

-- The record's values and the catalog's names (no personal data): board, mode, unit, value,
-- previousValue, carId, carName, boardId, routeId, routeName, and the throttle's board key.
ALTER TABLE public_event
  ADD COLUMN record jsonb CHECK (record IS NULL OR jsonb_typeof(record) = 'object'),
  ADD CONSTRAINT public_event_record_of_kind
    CHECK ((record IS NOT NULL) = (kind IN ('speed_record', 'route_record'))) NOT VALID;
ALTER TABLE public_event VALIDATE CONSTRAINT public_event_record_of_kind;
