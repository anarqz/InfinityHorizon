-- Platform v1, core lane (docs/contracts/CORE_API.md; ADR 0013, 0018, 0207, 0902, 0903): roles
-- and bans, the per-game switches of `game`, the notice board, Turnstile token reuse, the social
-- hub (comments, reports), the MCP grant store, and strikes per game.
--
-- Columns another lane's migration may add first (the gate lane's 20261019000100 adds
-- game.maintenance_until and sorts before this file) use IF NOT EXISTS, so either order of merge
-- applies cleanly.

-- Roles: player, moderator (MOD), admin (ADM). A ban is banned_at (+ ban_reason), permanent when
-- banned_until is NULL, else it ends at banned_until (readers compare with the database clock).
ALTER TABLE account DROP CONSTRAINT account_role_check;
ALTER TABLE account ADD CONSTRAINT account_role_check CHECK (role IN ('player', 'moderator', 'admin'));
ALTER TABLE account ADD COLUMN banned_until timestamptz;
ALTER TABLE account ADD CONSTRAINT account_ban_until_shape CHECK (banned_until IS NULL OR (banned_at IS NOT NULL AND banned_until > banned_at));

-- Per-game switches (ADR 0902 §2): maintenance (core's admin switch; the gate refuses new
-- admissions while maintenance_until > now), the version floors and whether the game accepts
-- players at all. The maintenance notice of the board is derived from these columns on read.
ALTER TABLE game ADD COLUMN IF NOT EXISTS maintenance_until timestamptz;
ALTER TABLE game ADD COLUMN IF NOT EXISTS maintenance_message text CHECK (char_length(maintenance_message) <= 500);
ALTER TABLE game ADD COLUMN IF NOT EXISTS min_hook text CHECK (char_length(min_hook) BETWEEN 1 AND 32);
ALTER TABLE game ADD COLUMN IF NOT EXISTS min_launcher text CHECK (char_length(min_launcher) BETWEEN 1 AND 32);
ALTER TABLE game ADD COLUMN IF NOT EXISTS accepting boolean NOT NULL DEFAULT true;
GRANT UPDATE (maintenance_until, maintenance_message) ON game TO fi_runtime;

-- The notice board (CORE_API.md §2.3): per game, or every game when game is NULL. Soft-deleted so
-- the audit row of the deletion keeps naming something.
CREATE TABLE notice (
  id         uuid PRIMARY KEY DEFAULT uuidv7(),
  game       text REFERENCES game (code),
  kind       text NOT NULL CHECK (kind IN ('news', 'maintenance', 'warning', 'event')),
  level      text NOT NULL CHECK (level IN ('info', 'warning', 'error')),
  title      text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 120 AND title !~ '[[:cntrl:]]'),
  -- Markdown, at most 4 KiB.
  body       text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 4096),
  lang       text NOT NULL CHECK (lang IN ('en', 'pt-BR')),
  since      timestamptz NOT NULL DEFAULT clock_timestamp(),
  until      timestamptz,
  created_by uuid NOT NULL REFERENCES account (id),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  deleted_at timestamptz,
  CONSTRAINT notice_window CHECK (until IS NULL OR until > since)
);

CREATE INDEX notice_board ON notice (lang, since DESC) WHERE deleted_at IS NULL;

GRANT SELECT, INSERT ON notice TO fi_runtime;
GRANT UPDATE (deleted_at) ON notice TO fi_runtime;
GRANT SELECT, DELETE ON notice TO fi_maintenance;

-- Turnstile tokens already presented (ADR 0903 §2): SHA-256 of the response token, kept 10
-- minutes (a token lives 300 s at Cloudflare). The inserting statement prunes a bounded batch of
-- expired rows, so the table stays small without a worker.
CREATE TABLE challenge_use (
  token_sha256 bytea PRIMARY KEY CHECK (length(token_sha256) = 32),
  used_at      timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE INDEX challenge_use_age ON challenge_use (used_at);

GRANT SELECT, INSERT, DELETE ON challenge_use TO fi_runtime;

-- Social hub (CORE_API.md §4). Profiles are listed publicly with a basic card unless the player
-- turns it off (OD-25: free players see basic cards of the server's players); social_hidden_at is
-- the three-report rule's or a moderator's hide (ADR 0204 §3).
ALTER TABLE profile ADD COLUMN social_listed boolean NOT NULL DEFAULT true;
ALTER TABLE profile ADD COLUMN social_hidden_at timestamptz;
CREATE INDEX profile_social_directory ON profile (game, lower(name)) WHERE status = 'active' AND social_listed AND social_hidden_at IS NULL;

CREATE TABLE social_comment (
  id                uuid PRIMARY KEY DEFAULT uuidv7(),
  game              text NOT NULL REFERENCES game (code),
  target_kind       text NOT NULL CHECK (target_kind IN ('player', 'club', 'news')),
  target_id         text NOT NULL CHECK (char_length(target_id) BETWEEN 1 AND 64),
  author_account_id uuid NOT NULL REFERENCES account (id),
  author_profile_id uuid NOT NULL REFERENCES profile (id),
  -- Plain text, at most 1 KiB.
  body              text NOT NULL CHECK (octet_length(body) BETWEEN 1 AND 1024),
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  hidden_at         timestamptz,
  deleted_at        timestamptz
);

CREATE INDEX social_comment_target ON social_comment (target_kind, target_id, id DESC) WHERE deleted_at IS NULL AND hidden_at IS NULL;
CREATE INDEX social_comment_author ON social_comment (author_account_id, created_at);

GRANT SELECT, INSERT ON social_comment TO fi_runtime;
GRANT UPDATE (hidden_at, deleted_at) ON social_comment TO fi_runtime;
GRANT SELECT, DELETE ON social_comment TO fi_maintenance;

-- Pages of comments, reports and audit rows are keyed by id alone (uuidv7: time-ordered and
-- exact, where an (instant, id) cursor in milliseconds could skip rows of the same millisecond).

-- Reports: one open report per reporter and target (a repeat counts once); three open reports by
-- distinct accounts hide the target pending review (ADR 0204 §3).
CREATE TABLE report (
  id                  uuid PRIMARY KEY DEFAULT uuidv7(),
  reporter_account_id uuid NOT NULL REFERENCES account (id),
  target_kind         text NOT NULL CHECK (target_kind IN ('player', 'comment')),
  target_id           text NOT NULL CHECK (char_length(target_id) BETWEEN 1 AND 64),
  -- The account behind the target (a profile's or a comment author's), for the moderation view.
  target_account_id   uuid REFERENCES account (id),
  reason              text NOT NULL CHECK (reason IN ('cheating', 'abuse', 'other')),
  text                text CHECK (char_length(text) BETWEEN 1 AND 1000),
  state               text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'dismissed', 'actioned')),
  created_at          timestamptz NOT NULL DEFAULT clock_timestamp(),
  decided_at          timestamptz,
  decided_by          uuid REFERENCES account (id),
  decision            text CHECK (decision IN ('dismiss', 'hide', 'strike', 'ban')),
  note                text CHECK (char_length(note) BETWEEN 1 AND 500),
  CONSTRAINT report_decision_shape CHECK ((state = 'open') = (decided_at IS NULL AND decided_by IS NULL AND decision IS NULL))
);

CREATE UNIQUE INDEX report_one_open ON report (reporter_account_id, target_kind, target_id) WHERE state = 'open';
CREATE INDEX report_open_target ON report (target_kind, target_id) WHERE state = 'open';
CREATE INDEX report_queue ON report (state, id);
CREATE INDEX report_target_account ON report (target_account_id) WHERE target_account_id IS NOT NULL;

GRANT SELECT, INSERT ON report TO fi_runtime;
GRANT UPDATE (state, decided_at, decided_by, decision, note) ON report TO fi_runtime;
GRANT SELECT ON report TO fi_maintenance;

-- Strikes per game (ADR 0902): game filled from the profile by a trigger, so the save core's
-- inserts stay unchanged; manual strikes come from moderators (created_by).
ALTER TABLE integrity_strike ADD COLUMN game text REFERENCES game (code);
UPDATE integrity_strike s SET game = p.game FROM profile p WHERE p.id = s.profile_id;

CREATE FUNCTION fi_strike_game() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  SELECT p.game INTO NEW.game FROM profile p WHERE p.id = NEW.profile_id;
  RETURN NEW;
END;
$$;

CREATE TRIGGER integrity_strike_game BEFORE INSERT ON integrity_strike FOR EACH ROW EXECUTE FUNCTION fi_strike_game();
ALTER TABLE integrity_strike ALTER COLUMN game SET NOT NULL;
ALTER TABLE integrity_strike DROP CONSTRAINT integrity_strike_source_check;
ALTER TABLE integrity_strike ADD CONSTRAINT integrity_strike_source_check CHECK (source IN ('save_gate', 'profile_gate', 'race_time', 'top_speed', 'manual'));
ALTER TABLE integrity_strike ADD COLUMN created_by uuid REFERENCES account (id);
ALTER TABLE integrity_strike ADD CONSTRAINT integrity_strike_manual_by CHECK ((source = 'manual') = (created_by IS NOT NULL));
CREATE INDEX integrity_strike_game ON integrity_strike (game, profile_id, created_at);

-- The operator audit (monetization's admin_action, ADR 0305 §3) also records account actors: the
-- MOD and ADM writes of /core/v1/admin and, later, MCP write tools (CORE_API.md §5). An operator
-- row (fi-billing's CLI) keeps its shape: actor 'admin:<name>' and a reason.
ALTER TABLE admin_action DROP CONSTRAINT admin_action_actor_check;
ALTER TABLE admin_action ADD CONSTRAINT admin_action_actor_check CHECK (actor ~ '^(admin|account):[A-Za-z0-9._@-]{1,128}$');
ALTER TABLE admin_action ALTER COLUMN reason DROP NOT NULL;
ALTER TABLE admin_action ADD COLUMN actor_kind text NOT NULL DEFAULT 'operator' CHECK (actor_kind IN ('operator', 'account'));
ALTER TABLE admin_action ADD COLUMN actor_account_id uuid REFERENCES account (id);
ALTER TABLE admin_action ADD COLUMN actor_role text CHECK (actor_role IN ('moderator', 'admin'));
ALTER TABLE admin_action ADD COLUMN target text CHECK (char_length(target) BETWEEN 1 AND 128);
ALTER TABLE admin_action ADD COLUMN request_id text CHECK (char_length(request_id) BETWEEN 1 AND 128);
ALTER TABLE admin_action ADD CONSTRAINT admin_action_actor_shape CHECK (
  CASE actor_kind
    WHEN 'operator' THEN reason IS NOT NULL AND actor_account_id IS NULL AND actor_role IS NULL
    ELSE actor_account_id IS NOT NULL AND actor_role IS NOT NULL AND target IS NOT NULL AND actor = 'account:' || actor_account_id::text
  END);
CREATE INDEX admin_action_actor ON admin_action (actor_account_id, at) WHERE actor_account_id IS NOT NULL;

-- MCP grant store (ADR 0018 §3): preconfigured clients, pending authorization requests (10
-- minutes, consumed once), connections (one per consent; role and scopes at consent), and
-- rotating refresh tokens (SHA-256 only; a replayed one revokes its family). Bounded by deletes of
-- expired rows inside the issuing transaction; nothing here is written before the OAuth flow
-- lands except by tests.
CREATE TABLE mcp_client (
  id            text PRIMARY KEY CHECK (id ~ '^[A-Za-z0-9._-]{1,64}$'),
  name          text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 100),
  redirect_uris text[] NOT NULL CHECK (cardinality(redirect_uris) BETWEEN 1 AND 8),
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE mcp_request (
  id              uuid PRIMARY KEY DEFAULT uuidv7(),
  client_id       text NOT NULL REFERENCES mcp_client (id),
  redirect_uri    text NOT NULL CHECK (char_length(redirect_uri) BETWEEN 1 AND 512),
  code_challenge  text NOT NULL CHECK (code_challenge ~ '^[A-Za-z0-9_-]{43}$'),
  scopes          text[] NOT NULL CHECK (scopes <@ ARRAY['platform:read', 'moderation:write']::text[] AND cardinality(scopes) >= 1),
  resource        text NOT NULL CHECK (char_length(resource) BETWEEN 1 AND 512),
  state           text CHECK (char_length(state) <= 512),
  expires_at      timestamptz NOT NULL,
  -- Consent: the account that approved, then the 60-second code (SHA-256), consumed once.
  account_id      uuid REFERENCES account (id),
  code_sha256     bytea CONSTRAINT mcp_request_code_key UNIQUE CHECK (length(code_sha256) = 32),
  code_expires_at timestamptz,
  consumed_at     timestamptz,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT mcp_request_code_shape CHECK ((code_sha256 IS NULL) = (code_expires_at IS NULL) AND (code_sha256 IS NULL OR account_id IS NOT NULL))
);

CREATE INDEX mcp_request_expiry ON mcp_request (expires_at);

CREATE TABLE mcp_connection (
  id               uuid PRIMARY KEY DEFAULT uuidv7(),
  account_id       uuid NOT NULL REFERENCES account (id),
  client_id        text NOT NULL REFERENCES mcp_client (id),
  role             text NOT NULL CHECK (role IN ('moderator', 'admin')),
  scopes           text[] NOT NULL CHECK (scopes <@ ARRAY['platform:read', 'moderation:write']::text[] AND cardinality(scopes) >= 1),
  granted_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  authenticated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  -- 30-day absolute life of a connection.
  expires_at       timestamptz NOT NULL,
  revoked_at       timestamptz,
  revoke_reason    text CHECK (revoke_reason IN ('player', 'role_changed', 'banned', 'replayed', 'admin')),
  generation       integer NOT NULL DEFAULT 1 CHECK (generation >= 1),
  CONSTRAINT mcp_connection_revoke_shape CHECK ((revoked_at IS NULL) = (revoke_reason IS NULL)),
  CONSTRAINT mcp_connection_life CHECK (expires_at > granted_at)
);

CREATE INDEX mcp_connection_account ON mcp_connection (account_id) WHERE revoked_at IS NULL;

CREATE TABLE mcp_refresh (
  token_sha256  bytea PRIMARY KEY CHECK (length(token_sha256) = 32),
  connection_id uuid NOT NULL REFERENCES mcp_connection (id),
  family_id     uuid NOT NULL,
  issued_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at    timestamptz NOT NULL,
  used_at       timestamptz,
  CONSTRAINT mcp_refresh_expiry CHECK (expires_at > issued_at)
);

CREATE INDEX mcp_refresh_connection ON mcp_refresh (connection_id);

GRANT SELECT ON mcp_client TO fi_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON mcp_request TO fi_runtime;
GRANT SELECT, INSERT, UPDATE ON mcp_connection, mcp_refresh TO fi_runtime;
GRANT DELETE ON mcp_refresh TO fi_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE ON mcp_client TO fi_maintenance;
GRANT SELECT, DELETE ON mcp_request, mcp_connection, mcp_refresh TO fi_maintenance;
