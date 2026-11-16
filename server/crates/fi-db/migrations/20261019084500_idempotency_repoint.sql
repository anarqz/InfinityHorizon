-- CR-L4 replay of answers that carry a secret (ACCOUNTS.md §1): a replay re-issues the family or
-- the identity ticket the client never received and re-points its record at what it just issued,
-- so the next replay revokes the latest one and at most one family lives per (scope, key). The
-- record stays append-only otherwise: an UPDATE that changes any column but resource_id is refused
-- for every role, and the runtime role may update resource_id only.
DROP TRIGGER idempotency_record_append_only ON idempotency_record;
CREATE TRIGGER idempotency_record_append_only BEFORE UPDATE ON idempotency_record FOR EACH ROW
  WHEN ((NEW.scope, NEW.key, NEW.route, NEW.request_sha256, NEW.status, NEW.response, NEW.created_at)
        IS DISTINCT FROM (OLD.scope, OLD.key, OLD.route, OLD.request_sha256, OLD.status, OLD.response, OLD.created_at))
  EXECUTE FUNCTION fi_forbid_update();

GRANT UPDATE (resource_id) ON idempotency_record TO fi_runtime;
