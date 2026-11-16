-- OD-106 follow-up: the admin console (fi_maintenance) sets the per-game client floor that
-- /launcher/v1/info and launcher policy already read (effective minimum = max(env, column)).
GRANT UPDATE (min_launcher, min_hook) ON game TO fi_maintenance;
