-- Retires the leaderboards and ghosts of /saves/v1 (UNIVERSE_API.md, change table 2026-10-27):
-- `board_entry` and `ghost` were the serving rows of `GET /saves/v1/leaderboards`, `rivals`,
-- `ghosts` and the `score` / `ghost` kinds of `POST /saves/v1/progress`, designed before the game's
-- RPC was ported. No client calls them and the game never fed them; the game's own boards and
-- ghosts are `game_score` and `game_ghost` (20261019120000_scores.sql), which stay, as do
-- `ghost_blob` (game_ghost names it), `player_position` (nearby) and `universe_refusal` (the
-- CR-L14 `min_time` refusals of `race_result`). The rows dropped here are derived: every fact that
-- wrote them stays in `progression_event`.

-- The mode-change purge iterates cloud_purge_target by regclass: its rows go before the tables, or
-- they would name dropped OIDs.
DELETE FROM cloud_purge_target
 WHERE table_name IN ('board_entry'::regclass, 'ghost'::regclass);

DROP TABLE ghost;
DROP TABLE board_entry;
