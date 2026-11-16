-- E-mail confirmation, password reset and the Firebase import of the `local` sign-in backend
-- (owner-decisions OD-122 follow-ups 2026-10-26 "the owner's answers to every open question" and
-- "Resend for e-mail"; ACCOUNTS.md §4.1 "Two identity backends"; register CR-S53). Expand only:
-- the previous build never reads the new columns and keeps writing rows it can still read.
--
-- 1. `email_verified_at`: when the address was confirmed (a confirmation link, a password reset
--    link, the operator's reset, or the Firebase import of an `emailVerified` user). Never set by a
--    sign-up while the instance sends no mail (FI_MAIL_BACKEND=log): such a row proved nothing and
--    must confirm once mail is on (security review H1). Of the rows created before this build, only
--    those that already have an FI account count as confirmed (their players are in); a bare
--    credential without an account must confirm, so a squatter on someone's address keeps nothing.
--    Never `updated_at`: that column means "the password changed" and ends older ID tokens.
ALTER TABLE credential ADD COLUMN email_verified_at timestamptz;
UPDATE credential SET email_verified_at = created_at
 WHERE id::text IN (SELECT idp_uid FROM account_identity);

-- 2. "Password not set" (a Firebase account without a password hash, e.g. Google-only): NULL, and
--    only the set-password mail gives it one. Imported Firebase hashes keep Firebase's modified
--    scrypt form `$firebase-scrypt$<salt b64>$<hash b64>` until the first successful sign-in
--    rehashes them to Argon2id (the project's hash parameters are configuration, never stored).
ALTER TABLE credential ALTER COLUMN password_hash DROP NOT NULL;
ALTER TABLE credential DROP CONSTRAINT credential_password_hash_check;
ALTER TABLE credential ADD CONSTRAINT credential_password_hash_check
  CHECK (password_hash IS NULL
         OR ((password_hash LIKE '$argon2id$%' OR password_hash LIKE '$firebase-scrypt$%')
             AND char_length(password_hash) <= 256));

-- 3. Single-use mail tokens (an opaque prefix and 256 random bits), only their SHA-256 stored.
--    `confirm` lives 24 hours, `reset` 1 hour. `used_at` is set once; a reset also spends the
--    credential's other open tokens. Rows go with their credential (erasure); `fi-cli
--    data-retention` deletes them 7 days after they expired or were used. They hold no address and
--    no secret.
CREATE TABLE credential_mail_token (
  token_sha256  bytea PRIMARY KEY CHECK (octet_length(token_sha256) = 32),
  credential_id uuid NOT NULL REFERENCES credential (id) ON DELETE CASCADE,
  purpose       text NOT NULL CHECK (purpose IN ('confirm', 'reset')),
  created_at    timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at    timestamptz NOT NULL,
  used_at       timestamptz,
  CONSTRAINT credential_mail_token_life CHECK (expires_at > created_at
                                               AND expires_at <= created_at + interval '1 day')
);
CREATE INDEX credential_mail_token_credential ON credential_mail_token (credential_id);

-- A password reset by mail revokes every family of the account.
ALTER TABLE refresh_token DROP CONSTRAINT refresh_token_revoke_reason_check;
ALTER TABLE refresh_token ADD CONSTRAINT refresh_token_revoke_reason_check
  CHECK (revoke_reason IN ('logout', 'reuse_detected', 'site_key_rotated', 'device_unbound', 'account_deleted', 'admin', 'replayed', 'deletion_requested', 'password_changed', 'password_reset'));

GRANT UPDATE (email_verified_at) ON credential TO fi_runtime;
GRANT SELECT, INSERT, UPDATE (used_at) ON credential_mail_token TO fi_runtime;
GRANT UPDATE (email_verified_at) ON credential TO fi_maintenance;
GRANT SELECT, DELETE ON credential_mail_token TO fi_maintenance;
