-- OD-79: only a short original-file startup scan. Reference dictionary is private server code.
-- No catalogue/import/signature/world policy, file inventory, telemetry hint or sanction table.
CREATE TABLE content_scan (
  id uuid PRIMARY KEY,
  owner uuid NOT NULL REFERENCES account(id),
  device uuid NOT NULL REFERENCES device_binding(id),
  game text NOT NULL REFERENCES game(code),
  session_id uuid NOT NULL,
  request_hash text NOT NULL CHECK(request_hash ~ '^[0-9a-f]{64}$'),
  body jsonb NOT NULL CHECK(octet_length(body::text)<=10000),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  expires_at timestamptz NOT NULL DEFAULT clock_timestamp()+interval '24 hours'
);
CREATE INDEX content_scan_quota ON content_scan(owner,created_at);
CREATE INDEX content_scan_expiry ON content_scan(expires_at);
-- Future ids deliberately have no play_session FK. First authenticated account/device owns it.
CREATE TABLE content_session_binding (
  session_id uuid PRIMARY KEY,
  owner uuid NOT NULL REFERENCES account(id),
  device uuid NOT NULL REFERENCES device_binding(id),
  game text NOT NULL REFERENCES game(code),
  scan_id uuid NOT NULL UNIQUE REFERENCES content_scan(id) ON DELETE CASCADE
);
GRANT SELECT,INSERT ON content_scan TO fi_runtime;
GRANT SELECT,INSERT,UPDATE ON content_session_binding TO fi_runtime;
GRANT SELECT,DELETE ON content_scan,content_session_binding TO fi_maintenance;
