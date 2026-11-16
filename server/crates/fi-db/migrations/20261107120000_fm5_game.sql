-- OD-206, CR-S97 (ACCOUNTS.md §2, GAME_PLATFORM.md §3a "Forza Motorsport 5"): the second game's row,
-- closed. No pool comes with it: a pool named `fm5` is configuration (FI_POOLS_JSON), and only the
-- local stack has one, so nobody can create an FM5 profile or session elsewhere.
INSERT INTO game (code, title, accepting) VALUES ('fm5', 'Forza Motorsport 5', false);
