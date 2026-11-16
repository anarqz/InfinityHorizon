-- SER-M3 step 7, the inbox lane (OD-48 item 2; docs/contracts/GAME_PLATFORM.md §3a "Supporter
-- social", register CR-S19): the game's in-game inbox (Xls Notifications.*) for Supporters, ported
-- from the Beta's SQLite `notifications` (deprecated/beta/server/src/db/notifications.rs), keyed by
-- (game, profile) instead of the XUID. Written and read by fi_db::inbox on the request's one
-- transaction: messages between friends (SendTextNotification), the platform's news (`notice`
-- rows, delivered lazily at RetrieveForUser) and the rivals sync (each friend's route time the
-- profile has not beaten, a TimeTrialTimeBeaten notification). Nothing of the Beta is imported
-- (OD-48). Free tickets never read or write these tables (their inbox stays the empty Pure reply).

-- 1. One row per notification of a profile's inbox. `id` is the wire's notification id
-- (XlsNotification.id, an i64 the game keeps; MarkRead and Clear name it). The parameter struct is
-- kept as its schema name and its JSON, encoded with the schema on delivery; the JSON may carry
-- keys of ours the encoder ignores (`noticeId` of a news item, `rivalBoard` / `rivalEvent` of a
-- rival time). A cleared row is a tombstone: never delivered again, and its `dedup_key` keeps a
-- news item or a rival time the player deleted from coming back (`cleared_by` 'player'); the rivals
-- sync revives only what it cleared itself ('server'). Expiry is 30 days after `sent_at` (the
-- Beta's TTL); fi_db::inbox prunes each profile to its newest rows (server-cleared tombstones
-- first) when it writes.
CREATE TABLE game_notification (
  game        text NOT NULL REFERENCES game (code),
  profile_id  uuid NOT NULL REFERENCES profile (id),
  id          bigint GENERATED ALWAYS AS IDENTITY NOT NULL,
  -- XlsNotification.notificationType (u16); the tab is (type >> 8) & 0x7f.
  type        integer NOT NULL CHECK (type BETWEEN 0 AND 65535),
  struct      text NOT NULL CHECK (struct ~ '^[A-Za-z]{1,64}$'),
  params      jsonb NOT NULL CHECK (jsonb_typeof(params) = 'object' AND pg_column_size(params) <= 8192),
  sent_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  read        boolean NOT NULL DEFAULT false,
  cleared     boolean NOT NULL DEFAULT false,
  cleared_by  text CHECK (cleared_by IN ('player', 'server')),
  -- A news item (`news:<notice id>`) or a rival time (`rival:<mode>:<sender>:<board>:<event>:
  -- <track>:<score bits>`): at most one row per profile.
  dedup_key   text CHECK (char_length(dedup_key) BETWEEN 1 AND 200),
  -- Who it is from: the friend of a message or of a rival time (none for news).
  sender_id   uuid REFERENCES profile (id),
  -- The request fact that sent a message (UUIDv5 of the play session and the request id,
  -- GAME_PLATFORM.md §3a): a replayed SendTextNotification finds its row and sends nothing new.
  source_id   uuid,
  PRIMARY KEY (game, id),
  CONSTRAINT game_notification_cleared CHECK (cleared = (cleared_by IS NOT NULL))
);

-- The inbox of a profile, newest first (delivery, MarkRead, Clear, pruning).
CREATE INDEX game_notification_inbox ON game_notification (game, profile_id, id);
CREATE UNIQUE INDEX game_notification_dedup ON game_notification (game, profile_id, dedup_key)
  WHERE dedup_key IS NOT NULL;
CREATE UNIQUE INDEX game_notification_source ON game_notification (game, source_id)
  WHERE source_id IS NOT NULL;
-- The hourly cap of a sender's messages (type 0x0200 with a sender).
CREATE INDEX game_notification_sender ON game_notification (game, sender_id, sent_at)
  WHERE sender_id IS NOT NULL AND type = 512;

-- 2. The inbox's sync state per profile: when RetrieveForUser last ran the sync (news and rivals;
-- skipped when under 25 s ago, the game polls every 30 s) and the newest news item delivered,
-- `(since, id)` of its `notice` row: a later poll looks only past it.
CREATE TABLE game_inbox (
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  synced_at  timestamptz,
  news_since timestamptz,
  news_id    uuid,
  PRIMARY KEY (game, profile_id)
);

-- 3. The rivals sync reads each friend's newest route times (score type 1, a track): a partial
-- index on the scoreboard lane's `game_score` (20261019120000_scores.sql), so a friend costs at
-- most the sync's per-friend bound, never a scan of their 5000 board keys.
CREATE INDEX game_score_route_recent ON game_score (game, mode, profile_id, submitted_at DESC)
  WHERE score_type = 1 AND track_id >= 0;

GRANT SELECT, INSERT, DELETE ON game_notification TO fi_runtime;
GRANT UPDATE (read, cleared, cleared_by) ON game_notification TO fi_runtime;
GRANT SELECT, INSERT ON game_inbox TO fi_runtime;
GRANT UPDATE (synced_at, news_since, news_id) ON game_inbox TO fi_runtime;
GRANT SELECT, DELETE ON game_notification, game_inbox TO fi_maintenance;
