-- Account erasure (ACCOUNTS.md §8; owner 2026-10-26 "Account erasure: build now"): the daily
-- `fi-cli account-erasure` job, fi_db::erasure, run as a fi_maintenance login (fi_jobs). One
-- account per transaction; what each table loses or keeps is the table of ACCOUNTS.md §8
-- "What erasure does per table" and fi_db::erasure::KEPT. Grants only: no table changes, no data
-- change. The DELETE and UPDATE grants it already had (20261017000200_accounts.sql and later:
-- identity, friends, game and universe rows, save revisions and parked bundles, content, crash,
-- MCP, device bindings, worlds) are not repeated here.

-- The erasure's own audit row and its account.deleted event (EVENTS.md §3.6).
GRANT INSERT ON account_audit, outbox_event TO fi_maintenance;

-- The erased account's join grants (they carry its XUID and network endpoints), its save line's
-- event log and its progression facts (append-only against UPDATE only).
GRANT DELETE ON mm_grant, save_event, progression_event TO fi_maintenance;

-- Strikes are moderation history and stay; their link to a parked bundle the erasure deletes is
-- cleared.
GRANT SELECT ON integrity_strike TO fi_maintenance;
GRANT UPDATE (parked_id) ON integrity_strike TO fi_maintenance;

-- Reports stay (moderation history); the free text an erased reporter wrote is cleared.
GRANT UPDATE (text) ON report TO fi_maintenance;

-- An erased profile's save head keeps no lease (an expired one may still be recorded on the row).
GRANT UPDATE (lease_session_id, lease_token, lease_device_id, lease_device_label,
              lease_acquired_at, lease_expires_at, lease_ttl_ms, lease_accounted_at)
  ON save_head TO fi_maintenance;

-- The account's audit rows that carry an install id (the binding's own is replaced at erasure,
-- so these would link the erased account to the install). The rest of its trail stays.
GRANT DELETE ON account_audit TO fi_maintenance;

-- The erasure's lookups that no index served (small tables today; plain CREATE INDEX inside the
-- migration's transaction).
CREATE INDEX mm_grant_account ON mm_grant (account_id);
CREATE INDEX crash_participant_xuid ON crash_participant (xuid);
CREATE INDEX crash_participant_account ON crash_participant (account_id);
CREATE INDEX integrity_strike_parked ON integrity_strike (parked_id) WHERE parked_id IS NOT NULL;
