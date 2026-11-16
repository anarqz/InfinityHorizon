-- Discord bot: the public event feed, the verified Discord link and the announce switch
-- (owner-decisions 2026-10-27 "Discord bot: channels, language and opt-out" and its refinement;
-- CORE_API.md §5a, ACCOUNTS.md §2/§8, register CR-S63). The server never calls Discord: an action
-- appends a `public_event` row in its own transaction, the bot reads them.
--
-- Expand only: new tables and NULL columns; the previous build neither reads nor writes them.

-- Per-profile feed state, outside `profile` so neither the switch nor the first admission bumps
-- the profile's row_version (its touch trigger) or takes its row lock during admission.
--   announce: the player's "announce me" switch (default on; no row = on).
--   first_admitted_at: the profile's first non-dedicated admission (`player_joined`, set once).
CREATE TABLE profile_feed (
  profile_id        uuid PRIMARY KEY REFERENCES profile (id),
  announce          boolean NOT NULL DEFAULT true,
  first_admitted_at timestamptz,
  changed_at        timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- Players admitted before this release have already joined: never announced.
INSERT INTO profile_feed (profile_id, first_admitted_at)
SELECT s.profile_id, min(s.admitted_at)
  FROM play_session s
 WHERE s.admitted_at IS NOT NULL AND NOT s.dedicated
 GROUP BY s.profile_id;

-- The verified Discord user of the account (one per account, at most one account per user). The
-- free-text `account.discord` (OD-106) stays the operator's note.
ALTER TABLE account
  ADD COLUMN discord_user_id      bigint CHECK (discord_user_id > 0),
  ADD COLUMN discord_username     text CHECK (char_length(discord_username) BETWEEN 2 AND 32),
  ADD COLUMN discord_global_name  text CHECK (char_length(discord_global_name) BETWEEN 1 AND 32),
  -- The avatar hash (`a_` = animated); NULL: Discord's default avatar. Shown as `avatarUrl`.
  ADD COLUMN discord_avatar       text CHECK (discord_avatar ~ '^(a_)?[0-9a-f]{32}$'),
  ADD COLUMN discord_linked_at    timestamptz,
  ADD COLUMN discord_refreshed_at timestamptz,
  ADD CONSTRAINT account_discord_link_shape CHECK (
    (discord_user_id IS NULL) = (discord_linked_at IS NULL)
    AND (discord_user_id IS NULL) = (discord_username IS NULL)
    AND (discord_user_id IS NULL) = (discord_refreshed_at IS NULL)
    AND (discord_user_id IS NOT NULL OR discord_global_name IS NULL)
    AND (discord_user_id IS NOT NULL OR discord_avatar IS NULL)) NOT VALID;
ALTER TABLE account VALIDATE CONSTRAINT account_discord_link_shape;
CREATE UNIQUE INDEX account_discord_user ON account (discord_user_id) WHERE discord_user_id IS NOT NULL;

-- One-time link codes (10 minutes, single use): only the SHA-256 of the code is kept.
CREATE TABLE discord_link_code (
  code_hash       bytea PRIMARY KEY CHECK (length(code_hash) = 32),
  account_id      uuid NOT NULL REFERENCES account (id),
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at      timestamptz NOT NULL,
  used_at         timestamptz,
  discord_user_id bigint,
  CONSTRAINT discord_link_code_used CHECK ((used_at IS NULL) = (discord_user_id IS NULL))
);
-- At most one unused code per account (issuing a new one deletes the old one first).
CREATE UNIQUE INDEX discord_link_code_open ON discord_link_code (account_id) WHERE used_at IS NULL;

-- The feed. `tx` orders the rows in commit-safe order: the reader returns only rows whose
-- transaction is older than every transaction still running (pg_snapshot_xmin), so a row that
-- commits late is never skipped. Kept 7 days (fi_db::retention).
CREATE TABLE public_event (
  id              bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  tx              xid8 NOT NULL DEFAULT pg_current_xact_id(),
  at              timestamptz NOT NULL DEFAULT clock_timestamp(),
  game            text REFERENCES game (code),
  kind            text NOT NULL CHECK (kind IN ('player_joined', 'lobby_opened', 'player_kicked', 'player_banned')),
  audience        text NOT NULL CHECK (audience IN ('public', 'staff')),
  subject_account uuid NOT NULL REFERENCES account (id),
  subject_profile uuid REFERENCES profile (id),
  subject_name    text,
  actor_account   uuid REFERENCES account (id),
  actor_profile   uuid REFERENCES profile (id),
  actor_name      text,
  until           timestamptz,
  CONSTRAINT public_event_audience_of_kind CHECK
    ((audience = 'staff') = (kind IN ('player_kicked', 'player_banned'))),
  CONSTRAINT public_event_public_names_profile CHECK (audience = 'staff' OR subject_profile IS NOT NULL)
);
CREATE INDEX public_event_cursor ON public_event (tx, id);
CREATE INDEX public_event_at ON public_event (at);
CREATE INDEX public_event_subject_profile ON public_event (subject_profile, kind, at);
CREATE INDEX public_event_actor_profile ON public_event (actor_profile) WHERE actor_profile IS NOT NULL;
CREATE INDEX public_event_subject_account ON public_event (subject_account);
CREATE INDEX public_event_actor_account ON public_event (actor_account) WHERE actor_account IS NOT NULL;

GRANT SELECT, INSERT, UPDATE, DELETE ON profile_feed TO fi_runtime;
GRANT SELECT, INSERT, UPDATE (used_at, discord_user_id), DELETE ON discord_link_code TO fi_runtime;
GRANT SELECT, INSERT, DELETE ON public_event TO fi_runtime;
-- Erasure keeps another player's kick or ban row and removes only the erased moderator from it.
GRANT UPDATE (actor_account, actor_profile, actor_name) ON public_event TO fi_runtime, fi_maintenance;
-- Retention (7 days) and erasure (fi_db::erasure::STEPS) as fi_maintenance.
GRANT SELECT, DELETE ON profile_feed, discord_link_code, public_event TO fi_maintenance;
