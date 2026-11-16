-- The 6-digit confirmation code of a `local` instance (owner-decisions OD-122 follow-up 2026-10-26
-- "e-mail confirmation by a 6-digit code in the launcher"; ACCOUNTS.md §4.1; register CR-S53).
-- The code rides on the confirmation mail's token row: same 24 hours, same single use, same
-- purpose. Only a keyed MAC is stored (HMAC-SHA256 under the instance's pepper, bound to the
-- row's `token_sha256`), so the table alone cannot test codes. `code_attempts` counts wrong
-- codes; the fifth spends the row. Expand only: the previous build never reads the columns.
ALTER TABLE credential_mail_token
  ADD COLUMN code_mac bytea CHECK (code_mac IS NULL OR octet_length(code_mac) = 32),
  ADD COLUMN code_attempts integer NOT NULL DEFAULT 0 CHECK (code_attempts BETWEEN 0 AND 5);

-- Wrong codes over the credential's life (security re-review M-C1): at 30 the code route refuses
-- until the address is confirmed by the link page (with the password) or a password reset, which
-- set it back to 0. New codes keep coming with each mail, so the per-code cap alone is no bound.
ALTER TABLE credential
  ADD COLUMN code_failures integer NOT NULL DEFAULT 0 CHECK (code_failures >= 0);

GRANT UPDATE (code_attempts) ON credential_mail_token TO fi_runtime;
GRANT UPDATE (code_failures) ON credential TO fi_runtime;
GRANT UPDATE (code_failures) ON credential TO fi_maintenance;
