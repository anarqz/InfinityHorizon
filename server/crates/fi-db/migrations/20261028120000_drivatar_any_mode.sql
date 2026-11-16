-- OD-128 (2026-10-28): the drivatars are served to every ticket from both modes. A candidates
-- query (fi_db::drivatars) now walks the trained drivatars of a game whatever their mode, most
-- recently trained first: this partial index is that walk (game_drivatar_recent keeps the
-- per-mode one).
CREATE INDEX game_drivatar_recent_any ON game_drivatar (game, trained_at DESC, profile_id) WHERE trained;
