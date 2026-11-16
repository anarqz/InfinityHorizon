-- SER-M3 step 6 (OD-48; GAME_PLATFORM.md §3a "Supporter universe"): the game plane's RPC gates
-- strike a profile for a refused report (fi_db::integrity::strike_rpc_in). Their sources beside
-- the save gate's: `profile_gate` (an UpdateAll beyond the growth allowances or the game's
-- limits), `race_time` and `top_speed` (as before), `xp_rate` (SubmitXpScore beyond the hourly
-- XP), `garage` (an UpdateAllTables with unknown cars or too many at once).
ALTER TABLE integrity_strike DROP CONSTRAINT integrity_strike_source_check;
ALTER TABLE integrity_strike ADD CONSTRAINT integrity_strike_source_check
  CHECK (source IN ('save_gate', 'profile_gate', 'race_time', 'top_speed', 'xp_rate', 'garage', 'manual'));
