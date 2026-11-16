-- The rivals sync moves a still-wanted rival time's sent_at to now once it is half its TTL old,
-- so it never expires while the friend's time stands (fi_db::inbox::sync_rivals_in, review fix).

GRANT UPDATE (sent_at) ON game_notification TO fi_runtime;
