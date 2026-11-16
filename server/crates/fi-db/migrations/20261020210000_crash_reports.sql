-- OD-78: metadata only. No dump table, causal attestations, victim threshold or auto-freeze.
CREATE TABLE crash_report (
  id uuid PRIMARY KEY,
  reporter uuid NOT NULL REFERENCES account(id),
  session_id uuid NOT NULL REFERENCES play_session(id),
  world_id uuid REFERENCES world(id),
  occurred_at timestamptz NOT NULL,
  idempotency_key uuid NOT NULL,
  request_hash bytea NOT NULL CHECK(octet_length(request_hash)=32),
  signature text NOT NULL CHECK(signature ~ '^[0-9a-f]{64}$'),
  metadata jsonb NOT NULL CHECK(octet_length(metadata::text)<=20000
    AND metadata->'consent'->>'metadata'='true'
    AND metadata->'consent'->>'minidump'='false'
    AND (NOT metadata ? 'dump' OR metadata->'dump'='null'::jsonb)),
  state text NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','analyzed')),
  classification text NOT NULL DEFAULT 'unknown' CHECK(classification IN ('unknown','needs_review','reviewed')),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at timestamptz NOT NULL DEFAULT clock_timestamp()+interval '30 days',
  UNIQUE(reporter,idempotency_key)
);
CREATE INDEX crash_report_queue ON crash_report(created_at) WHERE state='queued';
CREATE INDEX crash_report_quota ON crash_report(reporter,created_at);
CREATE INDEX crash_report_expiry ON crash_report(expires_at);
CREATE TABLE crash_participant (
  report_id uuid NOT NULL REFERENCES crash_report(id) ON DELETE CASCADE,
  account_id uuid NOT NULL REFERENCES account(id),
  xuid bigint NOT NULL,
  hinted boolean NOT NULL,
  PRIMARY KEY(report_id,account_id,xuid)
);
-- Only explicit human decisions, immutable except report-link removal at metadata expiry.
CREATE TABLE crash_review (
  id uuid PRIMARY KEY,
  report_id uuid REFERENCES crash_report(id) ON DELETE SET NULL,
  moderator uuid NOT NULL REFERENCES account(id),
  moderator_role text NOT NULL CHECK(moderator_role IN ('moderator','admin')),
  subject uuid REFERENCES account(id),
  private_case_digest bytea NOT NULL CHECK(octet_length(private_case_digest)=32),
  duration_s integer CHECK(duration_s BETWEEN 1 AND 86400),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  CHECK(duration_s IS NULL OR subject IS NOT NULL)
);
CREATE TABLE crash_restriction (
  id uuid PRIMARY KEY REFERENCES crash_review(id) ON DELETE CASCADE,
  account_id uuid NOT NULL REFERENCES account(id),
  until_at timestamptz NOT NULL,
  released_at timestamptz,
  released_by uuid REFERENCES account(id),
  CHECK((released_at IS NULL)=(released_by IS NULL))
);
CREATE INDEX crash_restriction_account ON crash_restriction(account_id,until_at) WHERE released_at IS NULL;
CREATE TABLE crash_notification (
  id uuid PRIMARY KEY DEFAULT uuidv7(),
  report_id uuid NOT NULL UNIQUE REFERENCES crash_report(id) ON DELETE CASCADE,
  classification text NOT NULL CHECK(classification IN ('unknown','needs_review','reviewed')),
  signature text NOT NULL CHECK(signature ~ '^[0-9a-f]{64}$'),
  human_review boolean NOT NULL DEFAULT false,
  attempts integer NOT NULL DEFAULT 0 CHECK(attempts BETWEEN 0 AND 8),
  next_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  lease_until timestamptz,
  generation integer NOT NULL DEFAULT 1,
  sent_generation integer NOT NULL DEFAULT 0
);
GRANT SELECT,INSERT ON crash_report,crash_participant TO fi_runtime;
GRANT SELECT ON crash_restriction TO fi_runtime;
GRANT SELECT,INSERT,UPDATE,DELETE ON crash_report,crash_participant,crash_notification TO fi_maintenance;
GRANT SELECT,INSERT,DELETE ON crash_review TO fi_maintenance;
GRANT SELECT,INSERT,UPDATE,DELETE ON crash_restriction TO fi_maintenance;
