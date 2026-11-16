-- ForzaInfinity schema, part 2 of 5: accounts, profiles, site keys, devices, refresh tokens, audit
-- (docs/contracts/ACCOUNTS.md, ADR 0006).
--
-- Accounts and profiles are never hard-deleted: deletion erases personal data (names) and keeps
-- the row as a tombstone, so foreign keys stay valid and a XUID is never handed out twice (Beta
-- reused the highest XUID after a delete: failure 14).

CREATE TABLE game (
  code  text PRIMARY KEY CHECK (code ~ '^[a-z][a-z0-9]{1,15}$'),
  title text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 64)
);

INSERT INTO game (code, title) VALUES ('fh2', 'Forza Horizon 2');

CREATE TABLE account (
  id                    uuid PRIMARY KEY DEFAULT uuidv7(),
  status                text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deletion_pending', 'deleted')),
  origin                text NOT NULL CHECK (origin IN ('registered', 'beta_import')),
  role                  text NOT NULL DEFAULT 'player' CHECK (role IN ('player', 'moderator')),
  -- Preferences the portal and the launcher share (BCP 47 tag, IANA zone); NULL = client default.
  locale                text CHECK (locale ~ '^[a-z]{2,3}(-[A-Za-z0-9]{2,8})*$'),
  time_zone             text CHECK (char_length(time_zone) BETWEEN 1 AND 64),
  created_at            timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at            timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version           bigint NOT NULL DEFAULT 1,
  -- Deletion has a grace period (14 days, ACCOUNTS.md §8): requested -> due -> erased.
  deletion_requested_at timestamptz,
  deletion_due_at       timestamptz,
  deleted_at            timestamptz,
  -- A ban is independent of the lifecycle: a banned player keeps the right to erasure.
  banned_at             timestamptz,
  ban_reason            text CHECK (char_length(ban_reason) <= 500),
  CONSTRAINT account_pending_has_due CHECK (status <> 'deletion_pending' OR (deletion_due_at IS NOT NULL AND deletion_requested_at IS NOT NULL)),
  CONSTRAINT account_deleted_when CHECK ((status = 'deleted') = (deleted_at IS NOT NULL)),
  CONSTRAINT account_ban_reason CHECK (banned_at IS NOT NULL OR ban_reason IS NULL)
);

CREATE TRIGGER account_touch BEFORE UPDATE ON account FOR EACH ROW EXECUTE FUNCTION fi_touch_row();
CREATE INDEX account_deletion_due ON account (deletion_due_at) WHERE status = 'deletion_pending';

-- The site key (XXXX-XXXX-XXXX, 60 random bits) is stored as HMAC-SHA256(pepper, canonical key);
-- the pepper lives in Secret Manager, versioned so it can rotate (pepper_version). A 60-bit key is
-- too short for a plain hash: without the pepper, a copy of this table (backup, PITR clone,
-- export) cannot be searched.
--
-- Imported Beta accounts: Beta stored SHA-1(canonical key) unsalted, which a copy of the table
-- lets anyone search in one pass. The importer never writes that hash here: it stores
-- legacy_wrapped = HMAC-SHA256(pepper, lower-case hex of Beta's SHA-1), verified at sign-in as
-- HMAC(pepper, hex(SHA-1(canonical(input)))) and replaced by key_hmac on that first use
-- (MIGRATION.md §4). Exactly one of the two is set.
CREATE TABLE account_site_key (
  account_id     uuid PRIMARY KEY REFERENCES account (id),
  key_hmac       bytea CHECK (key_hmac IS NULL OR length(key_hmac) = 32),
  legacy_wrapped bytea CHECK (legacy_wrapped IS NULL OR length(legacy_wrapped) = 32),
  pepper_version smallint NOT NULL CHECK (pepper_version > 0),
  issued_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version    bigint NOT NULL DEFAULT 1,
  CONSTRAINT account_site_key_one_hash CHECK (num_nonnulls(key_hmac, legacy_wrapped) = 1)
);

CREATE TRIGGER account_site_key_touch BEFORE UPDATE ON account_site_key FOR EACH ROW EXECUTE FUNCTION fi_touch_row();

-- New XUIDs start above every Beta XUID (Beta allocates MAX+1 above 2535000000000000), so both
-- allocators can run side by side during dual-run without colliding (fi_domain::xuid).
CREATE SEQUENCE profile_xuid_seq AS bigint START WITH 2535100000000001 MINVALUE 2535100000000001 NO CYCLE;

CREATE TABLE profile (
  id              uuid PRIMARY KEY DEFAULT uuidv7(),
  account_id      uuid NOT NULL REFERENCES account (id),
  game            text NOT NULL REFERENCES game (code),
  xuid            bigint NOT NULL CONSTRAINT profile_xuid_key UNIQUE CHECK (xuid > 2535000000000000),
  -- Gamertag Name#12345; both NULL once the profile is deleted (erased).
  name            text CHECK (name ~ '^[A-Za-z][A-Za-z0-9]{0,19}$'),
  discriminator   integer CHECK (discriminator BETWEEN 0 AND 99999),
  mode            text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  status          text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deletion_pending', 'deleted')),
  origin          text NOT NULL CHECK (origin IN ('created', 'beta_import')),
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version     bigint NOT NULL DEFAULT 1,
  mode_changed_at timestamptz,
  deletion_due_at timestamptz,
  deleted_at      timestamptz,
  CONSTRAINT profile_gamertag_parts CHECK ((name IS NULL) = (discriminator IS NULL)),
  CONSTRAINT profile_named_unless_deleted CHECK ((status = 'deleted') = (name IS NULL)),
  CONSTRAINT profile_deleted_when CHECK ((status = 'deleted') = (deleted_at IS NOT NULL)),
  CONSTRAINT profile_pending_has_due CHECK (status <> 'deletion_pending' OR deletion_due_at IS NOT NULL)
);

CREATE TRIGGER profile_touch BEFORE UPDATE ON profile FOR EACH ROW EXECUTE FUNCTION fi_touch_row();
-- Beta: UNIQUE(name COLLATE NOCASE, discriminator). Names are ASCII, so lower() is exact.
CREATE UNIQUE INDEX profile_gamertag_key ON profile (game, lower(name), discriminator) WHERE name IS NOT NULL;
CREATE INDEX profile_account ON profile (account_id);

-- A launcher install bound to an account. History is kept (unbound_at), because the sharing
-- limits count new bindings in a rolling window (monetization ADR 0304).
CREATE TABLE device_binding (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  account_id    uuid NOT NULL REFERENCES account (id),
  device_id     uuid NOT NULL,
  label         text NOT NULL CHECK (char_length(label) BETWEEN 1 AND 64),
  platform      text NOT NULL CHECK (platform IN ('windows', 'proton', 'linux')),
  bound_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  last_seen_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  unbound_at    timestamptz,
  unbind_reason text CHECK (unbind_reason IN ('player', 'admin', 'account_deleted', 'sharing_limit')),
  CONSTRAINT device_binding_unbind_shape CHECK ((unbound_at IS NULL) = (unbind_reason IS NULL))
);

CREATE UNIQUE INDEX device_binding_live ON device_binding (account_id, device_id) WHERE unbound_at IS NULL;
CREATE INDEX device_binding_window ON device_binding (account_id, bound_at);

-- Refresh tokens are opaque 256-bit randoms; only their SHA-256 is stored. Each use rotates the
-- token; presenting a used one again revokes its whole family (theft detection, ACCOUNTS.md §6).
CREATE TABLE refresh_token (
  id                uuid PRIMARY KEY DEFAULT uuidv7(),
  family_id         uuid NOT NULL,
  account_id        uuid NOT NULL REFERENCES account (id),
  device_binding_id uuid REFERENCES device_binding (id),
  client            text NOT NULL CHECK (client IN ('launcher', 'portal')),
  token_sha256      bytea NOT NULL CONSTRAINT refresh_token_hash_key UNIQUE CHECK (length(token_sha256) = 32),
  issued_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at        timestamptz NOT NULL,
  used_at           timestamptz,
  replaced_by       uuid REFERENCES refresh_token (id),
  revoked_at        timestamptz,
  revoke_reason     text CHECK (revoke_reason IN ('logout', 'reuse_detected', 'site_key_rotated', 'device_unbound', 'account_deleted', 'admin')),
  CONSTRAINT refresh_token_launcher_device CHECK (client <> 'launcher' OR device_binding_id IS NOT NULL),
  CONSTRAINT refresh_token_revoke_shape CHECK ((revoked_at IS NULL) = (revoke_reason IS NULL)),
  CONSTRAINT refresh_token_rotation_shape CHECK (replaced_by IS NULL OR used_at IS NOT NULL),
  CONSTRAINT refresh_token_expiry CHECK (expires_at > issued_at)
);

CREATE INDEX refresh_token_family ON refresh_token (family_id);
CREATE INDEX refresh_token_account_live ON refresh_token (account_id) WHERE revoked_at IS NULL AND used_at IS NULL;

-- Beta device tokens (SHA-1 of the 64-hex token in FH2Online_Accounts.ini), imported so the
-- launcher can exchange a Beta install's token for a refresh token once (ACCOUNTS.md §10). Unlike
-- site keys these are 256-bit random values, so their unsalted SHA-1 cannot be searched.
CREATE TABLE legacy_device_token (
  token_sha1   bytea PRIMARY KEY CHECK (length(token_sha1) = 20),
  profile_id   uuid NOT NULL REFERENCES profile (id),
  imported_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  exchanged_at timestamptz
);

CREATE INDEX legacy_device_token_profile ON legacy_device_token (profile_id);

-- VIP access is not stored here. Monetization owns the `entitlement` table (account_id, product,
-- status, access_until, version, ...; ADR 0305 §3), added by its own migration in this folder.
-- The account plane reads it under the rule of ACCOUNTS.md §7: VIP iff status IN ('active',
-- 'grace') AND access_until > now() (fi_domain::routing::Access::tier_at).

-- Account-level audit (sign-ins, key rotations, deletions, device changes). No IP addresses:
-- personal data the service does not need (LGPD art. 6 III); what the platform keeps instead is in
-- ARCHITECTURE.md §8.
CREATE TABLE account_audit (
  id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  account_id  uuid NOT NULL REFERENCES account (id),
  kind        text NOT NULL CHECK (kind ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$'),
  actor       text NOT NULL CHECK (actor IN ('player', 'system', 'moderator', 'import')),
  device_id   uuid,
  detail      jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(detail) = 'object'),
  occurred_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER account_audit_append_only BEFORE UPDATE ON account_audit FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();
CREATE INDEX account_audit_account ON account_audit (account_id, occurred_at);

-- Security-relevant rate limits, shared by every instance (ACCOUNTS.md §9): sign-in per gamertag
-- and per client network, account creation per client network and per device, site-key rotation
-- per account. Per-instance counters would multiply each limit by the instance count. The key is
-- '<limit>:' || hex(HMAC-SHA256(pepper, subject)), so no gamertag or IP prefix is stored in clear;
-- each row lives until an hour after its window (fi_maintenance prunes). Hot-path limits (10 per
-- second per account) stay per instance (approximate, documented).
CREATE TABLE rate_limit (
  key          text NOT NULL CHECK (key ~ '^[a-z][a-z0-9_.]*:[0-9a-f]{64}$'),
  window_start timestamptz NOT NULL,
  window_s     integer NOT NULL CHECK (window_s BETWEEN 1 AND 86400),
  count        integer NOT NULL CHECK (count > 0),
  PRIMARY KEY (key, window_start)
);

CREATE INDEX rate_limit_window ON rate_limit (window_start);

-- Idempotency of the account plane's writes that have no natural key (ACCOUNTS.md §1): POST
-- /accounts, /profiles, /me/site-key, save/parked, deletions. One row per (scope, key), written in
-- the business transaction (scope: the account, or the device on public routes). A replay with the
-- same request hash gets the recorded outcome; another body is REQUEST.IDEMPOTENCY_KEY_REUSED.
-- Responses carrying a secret (a new site key, tokens) are never stored: their replay re-issues the
-- secret and revokes the first one, which the client never received, so `response` holds only
-- bodies without secrets and `resource_id` names what the first request created. Pruned after 24 h.
CREATE TABLE idempotency_record (
  scope          text NOT NULL CHECK (scope ~ '^(account|device):[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
  key            uuid NOT NULL,
  route          text NOT NULL CHECK (char_length(route) BETWEEN 1 AND 64),
  request_sha256 bytea NOT NULL CHECK (length(request_sha256) = 32),
  status         smallint NOT NULL CHECK (status BETWEEN 200 AND 599),
  resource_id    uuid,
  response       jsonb CHECK (response IS NULL OR jsonb_typeof(response) = 'object'),
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (scope, key)
);

CREATE TRIGGER idempotency_record_append_only BEFORE UPDATE ON idempotency_record FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();
CREATE INDEX idempotency_record_age ON idempotency_record (created_at);

GRANT SELECT ON game TO fi_runtime, fi_maintenance;
GRANT SELECT, INSERT, UPDATE ON account, account_site_key, profile, device_binding, refresh_token, legacy_device_token TO fi_runtime;
GRANT SELECT, INSERT ON account_audit, idempotency_record TO fi_runtime;
GRANT SELECT, INSERT ON rate_limit TO fi_runtime;
GRANT UPDATE (count) ON rate_limit TO fi_runtime;
GRANT USAGE ON SEQUENCE profile_xuid_seq TO fi_runtime;
GRANT SELECT ON account, account_site_key, profile, device_binding, refresh_token, legacy_device_token, account_audit TO fi_maintenance;
-- Erasure and retention: expired refresh tokens, exchanged legacy tokens, a deleted account's
-- site key, old rate-limit windows and idempotency records. Accounts and profiles themselves are
-- never deleted (tombstones).
GRANT DELETE ON refresh_token, legacy_device_token, account_site_key TO fi_maintenance;
GRANT SELECT, DELETE ON rate_limit, idempotency_record TO fi_maintenance;
GRANT UPDATE ON account, profile, device_binding TO fi_maintenance;
