-- SER-M3 step 6, the scoreboard lane's review fixes (GAME_PLATFORM.md §3a "Supporter universe").
--
-- A board row's ghostFileId, and a rival's own ghost, is the recording of that row's score: the
-- board reads join game_ghost on the score's id, so a newer best without an upload shows no ghost
-- until its upload lands (one ghost per board key, so a score has at most one).
CREATE UNIQUE INDEX game_ghost_score ON game_ghost (game, score_id);
