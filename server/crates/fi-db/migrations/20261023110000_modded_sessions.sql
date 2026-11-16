-- Modded sessions (2026-10-23; OD-112 follow-up "modded sessions play online but post no board
-- results"; docs/contracts/GATE_API.md §2 "Modded sessions"). The launcher declares a session
-- modded from its launch plan (its multiplayer mod set is not empty): self-declared
-- compatibility evidence, not anti-cheat. Matchmaking treats the session as non-vanilla and the
-- boards and universe record none of its results; cloud saves, capacity and the queue are
-- unchanged. Written once at admission, never changed.
ALTER TABLE play_session ADD COLUMN modded boolean NOT NULL DEFAULT false;
