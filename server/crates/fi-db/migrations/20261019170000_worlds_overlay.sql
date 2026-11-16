-- Worlds in the overlay (2026-10-19; OD-54, OD-56, OD-57, OD-58; docs/contracts/MATCHMAKING_API.md
-- §1-§3.5, register CR-S21). Written by fi-matchmaking through fi_db::matchmaking; the invites
-- are also read and declined by core (CORE_API.md §4.2).
--
-- 1. world: the host chooses an audience (everyone, supporters, friends, private) instead of a
--    visibility; no password: friends and private worlds carry a server-generated 8-digit code,
--    unique among the open worlds of the game; the host's coarse location (a 1°x1° cell and a
--    region, never coordinates) and latency limit (under 60, 100 or 150 ms, OD-59); a world also closes when its host's play
--    session ends (`session_ended`). A closed world keeps no address, LAN address, peer, location
--    or code (the old closed rows are cleared here too).
DROP INDEX world_list;
ALTER TABLE world RENAME COLUMN visibility TO audience;
ALTER TABLE world DROP CONSTRAINT world_visibility_check;
-- public: everyone (a Supporter host's public world stays Supporter-only, as it was listed);
-- friends: a Supporter host's friends, else private; secret: private.
UPDATE world SET audience = CASE
    WHEN audience = 'public' AND kind = 'supporter' THEN 'supporters'
    WHEN audience = 'public' THEN 'everyone'
    WHEN audience = 'friends' AND kind = 'supporter' THEN 'friends'
    ELSE 'private'
  END;
ALTER TABLE world DROP COLUMN password_hash;
ALTER TABLE world
  ADD COLUMN code text CONSTRAINT world_code_shape CHECK (code ~ '^[0-9]{8}$'),
  ADD COLUMN max_rtt_ms integer CONSTRAINT world_max_rtt_ms_check CHECK (max_rtt_ms IN (60, 100, 150)),
  ADD COLUMN geo_cell integer CONSTRAINT world_geo_cell_check CHECK (geo_cell BETWEEN 0 AND 64799),
  ADD COLUMN region text CONSTRAINT world_region_check CHECK (region ~ '^[a-z]{2}(-[a-z]+)?$'),
  ALTER COLUMN endpoint_ip DROP NOT NULL;
UPDATE world SET code = lpad(floor(random() * 100000000)::bigint::text, 8, '0')
  WHERE closed_at IS NULL AND audience IN ('friends', 'private');
UPDATE world SET endpoint_ip = NULL, endpoint_lan = '{}', peers = '{}' WHERE closed_at IS NOT NULL;
ALTER TABLE world
  ADD CONSTRAINT world_audience_check CHECK (audience IN ('everyone', 'supporters', 'friends', 'private')),
  -- A free host chooses everyone or private (OD-57).
  ADD CONSTRAINT world_audience_follows_tier CHECK (kind = 'supporter' OR audience IN ('everyone', 'private')),
  ADD CONSTRAINT world_code_follows_audience
    CHECK (closed_at IS NOT NULL OR (code IS NOT NULL) = (audience IN ('friends', 'private'))),
  ADD CONSTRAINT world_closed_is_cleared
    CHECK (closed_at IS NULL OR (code IS NULL AND geo_cell IS NULL AND region IS NULL AND endpoint_ip IS NULL)),
  ADD CONSTRAINT world_open_has_endpoint CHECK (closed_at IS NOT NULL OR endpoint_ip IS NOT NULL);
ALTER TABLE world DROP CONSTRAINT world_close_reason_check;
ALTER TABLE world ADD CONSTRAINT world_close_reason_check
  CHECK (close_reason IN ('host_closed', 'expired', 'replaced', 'session_ended'));

-- The list reads the open worlds of one game and mode, newest first (OD-56, OD-58).
CREATE INDEX world_open_list ON world (game, mode, opened_at DESC, id) WHERE closed_at IS NULL;
CREATE UNIQUE INDEX world_open_code ON world (game, code) WHERE closed_at IS NULL AND code IS NOT NULL;
-- A play session's end closes its open worlds (fi_db::admission, the gate's leave and sweep).
CREATE INDEX world_open_session ON world (host_session_id) WHERE closed_at IS NULL;
-- Friends' worlds: the friends filter and a friend's presence.
CREATE INDEX world_open_host_account ON world (game, host_account_id) WHERE closed_at IS NULL;

-- 2. mm_grant: a seat reserved for 60 s and renewed up to its hold (`hold_until`, at most 15
--    minutes after issue); the game's arrival may add 60 s past the hold, once; how a seat ended.
ALTER TABLE mm_grant
  ADD COLUMN hold_until timestamptz,
  ADD COLUMN renewals integer NOT NULL DEFAULT 0 CONSTRAINT mm_grant_renewals_check CHECK (renewals >= 0),
  ADD COLUMN arrived_at timestamptz,
  ADD COLUMN end_reason text CONSTRAINT mm_grant_end_reason_check
    CHECK (end_reason IN ('expired', 'released', 'kicked', 'world_closed'));
UPDATE mm_grant SET hold_until = expires_at;
UPDATE mm_grant SET end_reason = state WHERE state NOT IN ('issued', 'used');
ALTER TABLE mm_grant
  ALTER COLUMN hold_until SET NOT NULL,
  DROP CONSTRAINT mm_grant_ttl,
  ADD CONSTRAINT mm_grant_hold CHECK (
    expires_at > issued_at
    AND hold_until > issued_at
    AND hold_until <= issued_at + interval '15 minutes'
    AND expires_at <= hold_until + interval '60 seconds'),
  ADD CONSTRAINT mm_grant_end_shape CHECK ((end_reason IS NULL) = (state IN ('issued', 'used')));

-- The seat read's "how did my last seat end".
CREATE INDEX mm_grant_profile_ended ON mm_grant (game, profile_id, ended_at DESC) WHERE ended_at IS NOT NULL;
-- Friends seated in a world (the friends filter).
CREATE INDEX mm_grant_world_used ON mm_grant (world_id, account_id) WHERE state = 'used';

-- 3. No world queue (OD-58): the gate's admission queue is the only one.
DROP TABLE mm_queue;

-- 4. Invites to play (OD-56) and requests to join (OD-59), one table: an `invite` goes from a
--    Supporter host's profile to a friend account; a `request` goes from a Supporter's profile
--    (`from_profile_id`, who gets the seat) to the world's host account. `to_account_id` is who
--    answers. A pending one holds a seat for about two minutes; a world has at most a few pending
--    (fi_db::matchmaking::MAX_PENDING). An accepted invite keeps letting its invitee in until
--    `expires_at` (OD-57: for its lifetime).
CREATE TABLE world_invite (
  id              uuid PRIMARY KEY,
  game            text NOT NULL REFERENCES game (code),
  kind            text NOT NULL CHECK (kind IN ('invite', 'request')),
  world_id        uuid NOT NULL REFERENCES world (id),
  from_account_id uuid NOT NULL REFERENCES account (id),
  from_profile_id uuid NOT NULL REFERENCES profile (id),
  to_account_id   uuid NOT NULL REFERENCES account (id),
  state           text NOT NULL DEFAULT 'pending'
                  CHECK (state IN ('pending', 'accepted', 'declined', 'expired', 'withdrawn')),
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at      timestamptz NOT NULL,
  ended_at        timestamptz,
  grant_id        uuid REFERENCES mm_grant (id),
  CONSTRAINT world_invite_not_self CHECK (from_account_id <> to_account_id),
  CONSTRAINT world_invite_ttl CHECK (expires_at > created_at AND expires_at <= created_at + interval '15 minutes'),
  CONSTRAINT world_invite_ended_shape CHECK ((state = 'pending') = (ended_at IS NULL)),
  CONSTRAINT world_invite_grant_shape CHECK (grant_id IS NULL OR state = 'accepted')
);

CREATE UNIQUE INDEX world_invite_one_pending ON world_invite (world_id, kind, to_account_id, from_account_id)
  WHERE state = 'pending';
CREATE INDEX world_invite_inbox ON world_invite (to_account_id, created_at DESC) WHERE state = 'pending';
CREATE INDEX world_invite_world ON world_invite (world_id, expires_at) WHERE state IN ('pending', 'accepted');
CREATE INDEX world_invite_expiry ON world_invite (expires_at) WHERE state = 'pending';
CREATE INDEX world_invite_sent ON world_invite (from_account_id, created_at DESC);

-- 5. A friend's latest play session (CORE_API.md §4.1 presence: the profile played last).
CREATE INDEX play_session_account_history ON play_session (account_id, created_at DESC);

GRANT SELECT, INSERT, UPDATE ON world_invite TO fi_runtime;
-- A session's end (gate-sweep may run as a maintenance login) withdraws the invites of its worlds.
GRANT SELECT, UPDATE, DELETE ON world_invite TO fi_maintenance;
