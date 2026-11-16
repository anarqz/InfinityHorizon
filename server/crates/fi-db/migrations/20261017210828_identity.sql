-- ForzaInfinity schema: Identity Platform sign-in (OD-20, ADR 0901, docs/contracts/ACCOUNTS.md §4.1,
-- §5; PORTAL_API.md §3). Identity Platform proves who a person is; these tables map its uid to an
-- account and hold the short-lived, single-use credentials of the exchange: identity tickets,
-- launcher authorization codes (loopback + PKCE) and device codes. Every secret is stored as its
-- SHA-256 only; the runtime role spends one with an UPDATE of its used/state column under
--   WHERE used_at IS NULL AND expires_at > clock_timestamp()
-- so two concurrent redemptions of one credential get exactly one row back. Nothing here is ever
-- DELETEd by fi_runtime: fi_maintenance prunes expired rows (and, at purge, the mapping).

-- One Identity Platform uid per account and one account per uid (ACCOUNTS.md §4.1). The e-mail is
-- kept for account notices and support only: never shown to other players, never in events or logs,
-- erased at purge (§8).
CREATE TABLE account_identity (
  account_id      uuid PRIMARY KEY REFERENCES account (id),
  idp_uid         text NOT NULL CONSTRAINT account_identity_uid_key UNIQUE CHECK (char_length(idp_uid) BETWEEN 1 AND 128),
  -- Every sign-in provider seen for this uid (google.com, password, ...), first one first.
  provider_ids    text[] NOT NULL CHECK (cardinality(provider_ids) BETWEEN 1 AND 16),
  email_lower     text CHECK (email_lower IS NULL OR (char_length(email_lower) BETWEEN 3 AND 320 AND email_lower = lower(email_lower))),
  email_verified  boolean NOT NULL DEFAULT false,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
  last_sign_in_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- Identity tickets (`fi_it_...`, 15 minutes, single use): the answer to an exchange whose uid has no
-- account yet, redeemed by account creation or a legacy link. Bound to the uid, the client kind and
-- the auth_time of the ID token they came from, so a later "recent sign-in" check holds without an
-- ID token. `id` names the ticket in idempotency records (never the secret).
CREATE TABLE identity_ticket (
  ticket_sha256  bytea PRIMARY KEY CHECK (length(ticket_sha256) = 32),
  id             uuid NOT NULL DEFAULT uuidv7() CONSTRAINT identity_ticket_id_key UNIQUE,
  idp_uid        text NOT NULL CHECK (char_length(idp_uid) BETWEEN 1 AND 128),
  provider_id    text NOT NULL CHECK (char_length(provider_id) BETWEEN 1 AND 64),
  email_lower    text CHECK (email_lower IS NULL OR char_length(email_lower) BETWEEN 3 AND 320),
  email_verified boolean NOT NULL,
  client         text NOT NULL CHECK (client IN ('launcher', 'portal')),
  auth_time      timestamptz NOT NULL,
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at     timestamptz NOT NULL,
  used_at        timestamptz,
  CONSTRAINT identity_ticket_expiry CHECK (expires_at > created_at)
);

CREATE INDEX identity_ticket_expiry_idx ON identity_ticket (expires_at);

-- Launcher authorization codes (32 base32 characters, 60 s, single use), minted by the portal's
-- authorization page (POST /portal/v1/auth/launcher-codes) and redeemed by the launcher with its
-- PKCE verifier and the exact loopback redirect (POST /launcher/v1/auth/token, RFC 8252, RFC 7636).
-- A code redeemed as `reauth` for a deletion proves a recent sign-in through auth_time.
CREATE TABLE launcher_auth_code (
  code_sha256    bytea PRIMARY KEY CHECK (length(code_sha256) = 32),
  id             uuid NOT NULL DEFAULT uuidv7() CONSTRAINT launcher_auth_code_id_key UNIQUE,
  idp_uid        text NOT NULL CHECK (char_length(idp_uid) BETWEEN 1 AND 128),
  provider_id    text NOT NULL CHECK (char_length(provider_id) BETWEEN 1 AND 64),
  email_lower    text CHECK (email_lower IS NULL OR char_length(email_lower) BETWEEN 3 AND 320),
  email_verified boolean NOT NULL,
  auth_time      timestamptz NOT NULL,
  -- base64url(SHA-256(code_verifier)), S256 only.
  code_challenge text NOT NULL CHECK (code_challenge ~ '^[A-Za-z0-9_-]{43}$'),
  -- Loopback only, never localhost, never another path (ACCOUNTS.md §5).
  redirect_uri   text NOT NULL CHECK (redirect_uri ~ '^http://127\.0\.0\.1:[0-9]{1,5}/callback$'),
  device_label   text NOT NULL CHECK (char_length(device_label) BETWEEN 1 AND 64),
  created_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at     timestamptz NOT NULL,
  used_at        timestamptz,
  CONSTRAINT launcher_auth_code_expiry CHECK (expires_at > created_at)
);

CREATE INDEX launcher_auth_code_expiry_idx ON launcher_auth_code (expires_at);

-- Device codes (RFC 8628) for a launcher without a usable browser (Steam Deck Game Mode): the
-- launcher holds the device code (43 base64url characters, SHA-256 stored), the player types the
-- user code (XXXX-XXXX of the site-key alphabet, stored canonical without the dash) on
-- <portal>/link, a signed-in visitor approves it, the launcher's next poll redeems it once.
-- last_poll_at enforces the polling interval across instances.
CREATE TABLE device_authorization (
  device_code_sha256 bytea PRIMARY KEY CHECK (length(device_code_sha256) = 32),
  id                 uuid NOT NULL DEFAULT uuidv7() CONSTRAINT device_authorization_id_key UNIQUE,
  user_code          text NOT NULL CHECK (user_code ~ '^[2-9A-HJ-NP-Z]{8}$'),
  device_id          uuid NOT NULL,
  label              text NOT NULL CHECK (char_length(label) BETWEEN 1 AND 64),
  platform           text NOT NULL CHECK (platform IN ('windows', 'proton', 'linux')),
  state              text NOT NULL DEFAULT 'pending' CHECK (state IN ('pending', 'approved', 'denied', 'redeemed')),
  -- The approving identity (set together on approval).
  idp_uid            text CHECK (char_length(idp_uid) BETWEEN 1 AND 128),
  provider_id        text CHECK (char_length(provider_id) BETWEEN 1 AND 64),
  email_lower        text CHECK (email_lower IS NULL OR char_length(email_lower) BETWEEN 3 AND 320),
  email_verified     boolean,
  auth_time          timestamptz,
  interval_s         integer NOT NULL DEFAULT 5 CHECK (interval_s BETWEEN 1 AND 60),
  created_at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at         timestamptz NOT NULL,
  approved_at        timestamptz,
  redeemed_at        timestamptz,
  last_poll_at       timestamptz,
  CONSTRAINT device_authorization_expiry CHECK (expires_at > created_at),
  CONSTRAINT device_authorization_identity_shape CHECK (
    (state IN ('approved', 'redeemed')) = (idp_uid IS NOT NULL AND provider_id IS NOT NULL AND email_verified IS NOT NULL AND auth_time IS NOT NULL AND approved_at IS NOT NULL)
  ),
  CONSTRAINT device_authorization_redeemed_shape CHECK ((state = 'redeemed') = (redeemed_at IS NOT NULL))
);

-- A user code names one pending authorization (2^40 codes: a collision makes the insert retry).
CREATE UNIQUE INDEX device_authorization_pending_user_code ON device_authorization (user_code) WHERE state = 'pending';
CREATE INDEX device_authorization_expiry_idx ON device_authorization (expires_at);

-- Legacy link spends a Beta site key (ACCOUNTS.md §4.1, §10). The runtime role may not DELETE and
-- the row must keep exactly one 32-byte hash (account_site_key_one_hash), so the link overwrites the
-- hash with random bytes (nothing of the key survives) and sets spent_at; the verifier refuses spent
-- rows and fi_maintenance deletes them with the account's other personal data.
ALTER TABLE account_site_key ADD COLUMN spent_at timestamptz;

-- When a refresh token or a portal session token was last presented (the portal's sliding window,
-- /me/sessions' lastSeenAt).
ALTER TABLE refresh_token ADD COLUMN last_used_at timestamptz;

-- Two revocation reasons of the identity plane: `replayed` (a replayed POST /accounts, /auth/token or
-- /legacy/link re-issued the secret the client never received, CR-L4; the first family dies) and
-- `deletion_requested` (ACCOUNTS.md §8: every family but the caller's).
ALTER TABLE refresh_token DROP CONSTRAINT refresh_token_revoke_reason_check;
ALTER TABLE refresh_token ADD CONSTRAINT refresh_token_revoke_reason_check
  CHECK (revoke_reason IN ('logout', 'reuse_detected', 'site_key_rotated', 'device_unbound', 'account_deleted', 'admin', 'replayed', 'deletion_requested'));

-- Portal identity routes run before any account exists (POST /portal/v1/sessions, /accounts,
-- /auth/launcher-codes, ...): their idempotency scope is the identity, as the hex HMAC of its uid
-- under the site-key pepper, so no uid is stored in clear.
ALTER TABLE idempotency_record DROP CONSTRAINT idempotency_record_scope_check;
ALTER TABLE idempotency_record ADD CONSTRAINT idempotency_record_scope_check
  CHECK (scope ~ '^((account|device):[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|identity:[0-9a-f]{64})$');

GRANT SELECT, INSERT, UPDATE ON account_identity, identity_ticket, launcher_auth_code, device_authorization TO fi_runtime;
GRANT SELECT ON account_identity, identity_ticket, launcher_auth_code, device_authorization TO fi_maintenance;
-- Pruning of expired credentials, and erasure of the mapping at purge (ACCOUNTS.md §8).
GRANT DELETE ON account_identity, identity_ticket, launcher_auth_code, device_authorization TO fi_maintenance;
