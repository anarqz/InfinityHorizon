-- OD-52: a Hardcore -> Noob change also deletes the profile's in-game inbox (20261019140200_inbox.sql):
-- the notifications it received (`inbox` in the warning) and its sync state. Messages it sent stay
-- in their recipients' inboxes (theirs); its rival notifications in friends' inboxes are cleared by
-- their next rivals sync (a Noob profile is no longer a Hardcore viewer's friend there).
INSERT INTO cloud_purge_target (table_name, column_name, category) VALUES
  ('game_notification', 'profile_id', 'inbox'),
  ('game_inbox', 'profile_id', NULL);
