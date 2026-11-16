-- The review of OD-52 (2026-10-19): the Xbox Live presence a profile's game posts
-- (game_player_presence) is written for every tier, free sessions included, and is transient:
-- what friends see while the profile plays, overwritten by its next post. It is not cloud data
-- (GAME_PLATFORM.md §3a "free state"), so a Hardcore -> Noob change no longer deletes it; modes
-- never meet on reads instead (fi_db::gameplane::presence_in filters by the viewer's mode).
DELETE FROM cloud_purge_target WHERE table_name = 'game_player_presence'::regclass;
