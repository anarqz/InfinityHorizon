-- OD-141 (2026-10-29, owner): "Criaremos uma flag na criação do lobby. HARDCORE / NOOB / QUALQUER UM."
-- A world keeps its host's `mode`; `any_mode` marks one the host opened to Hardcore and Noob
-- (`modes: "any"`, MATCHMAKING_API.md §2.5, register CR-S76). Every existing world stays single-mode.
ALTER TABLE world ADD COLUMN any_mode boolean NOT NULL DEFAULT false;

-- The list's second scan (`fi_db::matchmaking::LIST_SQL`): the newest open `any` worlds of the
-- other mode, newest first. Few worlds are mixed, so one partial index serves both `modded` sides.
CREATE INDEX world_open_any ON world (game, opened_at DESC, id)
  WHERE closed_at IS NULL AND any_mode;
