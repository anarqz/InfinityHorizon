-- OD-130 (2026-10-28): drivatars are served within the caller's mode again (Hardcore never races
-- Noob drivatars). The any-mode walk of 20261028120000 is gone; `game_drivatar_recent` (game,
-- mode) is the candidates' index again.
DROP INDEX IF EXISTS game_drivatar_recent_any;
