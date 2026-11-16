-- OD-82 / ONLINE_CONTROL.md: global online stop, default open until explicitly changed.
-- Runtime services only read it. Maintenance jobs mutate it; no player data is retained.
CREATE TABLE online_control (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    status text NOT NULL CHECK (status IN ('open', 'closed')),
    revision bigint NOT NULL CHECK (revision >= 0),
    reason text,
    closed_at timestamptz,
    CHECK ((status = 'open' AND reason IS NULL AND closed_at IS NULL)
        OR (status = 'closed' AND reason IS NOT NULL AND reason IN ('budget', 'operator')
            AND closed_at IS NOT NULL))
);
CREATE TABLE online_control_audit (
    revision bigint PRIMARY KEY CHECK (revision > 0),
    status text NOT NULL CHECK (status IN ('open', 'closed')),
    reason text,
    changed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK ((status = 'open' AND reason IS NULL)
        OR (status = 'closed' AND reason IS NOT NULL AND reason IN ('budget', 'operator')))
);
CREATE FUNCTION fi_online_control_before_update() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.singleton IS DISTINCT FROM OLD.singleton OR NEW.revision <> OLD.revision + 1 THEN
        RAISE EXCEPTION 'online control revision must advance exactly once'
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END
$$;
CREATE FUNCTION fi_online_control_after_update() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO online_control_audit (revision, status, reason)
        VALUES (NEW.revision, NEW.status, NEW.reason);
    DELETE FROM online_control_audit WHERE revision <= NEW.revision - 256;
    RETURN NEW;
END
$$;
CREATE TRIGGER online_control_revision BEFORE UPDATE ON online_control
    FOR EACH ROW EXECUTE FUNCTION fi_online_control_before_update();
CREATE TRIGGER online_control_record AFTER UPDATE ON online_control
    FOR EACH ROW EXECUTE FUNCTION fi_online_control_after_update();
CREATE TRIGGER online_control_audit_immutable BEFORE UPDATE ON online_control_audit
    FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();
INSERT INTO online_control (status, revision) VALUES ('open', 0);
GRANT SELECT ON online_control TO fi_runtime, fi_maintenance;
GRANT UPDATE ON online_control TO fi_maintenance;
GRANT SELECT, INSERT, DELETE ON online_control_audit TO fi_maintenance;
