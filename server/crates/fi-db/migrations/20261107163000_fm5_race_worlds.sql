-- CR-S99 (FM5_LOBBIES.md §1, OD-206): FM5's private race lobbies on official Worlds. `race` is an
-- official world's activity, as `carmeet` (a player world's stays `freeroam` | `roadtrip`), and a
-- world takes its game's seats: 16 for a game other than FH2, FH2's rows stay at 12
-- (`fi_domain::GameRules::max_players`). No other table bounds seats (mm_grant, world_invite and
-- world_access count rows). Forward only: applied migrations are immutable.

ALTER TABLE world DROP CONSTRAINT world_activity_known;
ALTER TABLE world
  ADD CONSTRAINT world_activity_known CHECK (
    activity IN ('freeroam', 'roadtrip') OR (activity IN ('carmeet', 'race') AND host_kind = 'official')
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_activity_known;

ALTER TABLE world
  DROP CONSTRAINT world_players_check,
  DROP CONSTRAINT world_peak_players_check,
  DROP CONSTRAINT world_max_players_check;
ALTER TABLE world
  ADD CONSTRAINT world_seats_by_game CHECK (
    players BETWEEN 0 AND 16 AND peak_players BETWEEN 0 AND 16 AND max_players BETWEEN 1 AND 16
    AND (game <> 'fh2' OR (players <= 12 AND peak_players <= 12 AND max_players <= 12))
  ) NOT VALID;
ALTER TABLE world VALIDATE CONSTRAINT world_seats_by_game;
