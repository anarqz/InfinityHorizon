-- OD-191 live re-test (2026-11-03): a car meet seat whose player a group world took over stays in the
-- car meet world's punches until the game reaches the group world, so the engine keeps the peer while
-- the game reads the new session. The seat itself ends `released` when the group seat is reserved
-- (one live seat per profile, mm_grant_one_live); `moving_to` names that group seat and `moving_until`
-- caps the listing at 60 s. Listed only while the group seat is still `issued` and unexpired
-- (fi_db::matchmaking::OFFICIAL_PUNCHES_SQL). Forward only: applied migrations are immutable.

ALTER TABLE mm_grant
  ADD COLUMN moving_to uuid REFERENCES mm_grant (id) ON DELETE SET NULL,
  ADD COLUMN moving_until timestamptz;

CREATE INDEX mm_grant_world_moving ON mm_grant (world_id, moving_until) WHERE moving_to IS NOT NULL;
