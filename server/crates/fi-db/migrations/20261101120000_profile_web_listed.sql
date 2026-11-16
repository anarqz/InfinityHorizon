-- The play dashboard's opt-out (OD-168 follow-up DB-Q4, ACCOUNTS.md §5 `POST /profiles/{xuid}/website`,
-- register CR-S83): every active, unbanned profile is listed on play.infinity-horizon.com unless its
-- player turns it off in the launcher. `false` hides the profile from every dashboard list and page
-- (DASHBOARD_API.md §1). Not a moderation flag: `social_hidden_at` and `social_listed` are unchanged.
--
-- Expand only: a column with a default the previous build never reads; fi_runtime already holds
-- UPDATE on profile (20261017000200_accounts.sql).
ALTER TABLE profile ADD COLUMN web_listed boolean NOT NULL DEFAULT true;
