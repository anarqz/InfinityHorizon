-- OD-122 M2.3, the `local` sign-in backend (ACCOUNTS.md §4.1 "Two identity backends", ADR 0023 §9,
-- CR-S48): a `local` instance is its own identity provider. One credential per normalized e-mail:
-- the address trimmed and lower-cased (3-254 characters, exactly one `@`, no whitespace) and the
-- password's Argon2id PHC string (never the password). The credential's id is the `sub` of the
-- instance's ID tokens, so `account_identity.idp_uid` names it after `POST /launcher/v1/accounts`
-- exactly as it names an Identity Platform uid. Never in events or logs. Expand only: a `firebase`
-- instance never writes it.
CREATE TABLE credential (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  email_lower   text NOT NULL CONSTRAINT credential_email_key UNIQUE
                CHECK (char_length(email_lower) BETWEEN 3 AND 254
                       AND email_lower = lower(email_lower)
                       AND email_lower = btrim(email_lower)
                       AND char_length(email_lower) - char_length(replace(email_lower, '@', '')) = 1),
  -- `$argon2id$v=19$m=…,t=…,p=…$<salt>$<hash>`.
  password_hash text NOT NULL CHECK (password_hash LIKE '$argon2id$%' AND char_length(password_hash) <= 256),
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT credential_updated_after_created CHECK (updated_at >= created_at)
);

-- A password change (POST /launcher/v1/auth/password) revokes every family of the account.
ALTER TABLE refresh_token DROP CONSTRAINT refresh_token_revoke_reason_check;
ALTER TABLE refresh_token ADD CONSTRAINT refresh_token_revoke_reason_check
  CHECK (revoke_reason IN ('logout', 'reuse_detected', 'site_key_rotated', 'device_unbound', 'account_deleted', 'admin', 'replayed', 'deletion_requested', 'password_changed'));

-- The runtime signs up, reads and changes passwords; it never deletes a credential. The operator's
-- reset (`fi-platform accounts reset-password`) and erasure run as fi_maintenance.
GRANT SELECT, INSERT ON credential TO fi_runtime;
GRANT UPDATE (password_hash, updated_at) ON credential TO fi_runtime;
GRANT SELECT, UPDATE (password_hash, updated_at), DELETE ON credential TO fi_maintenance;
