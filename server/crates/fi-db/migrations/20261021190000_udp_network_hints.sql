-- CR-S28: authenticated HTTPS rendezvous. No backfill: legacy grants have no session binding
-- and cannot publish or expose network hints. Migration 45's proof credentials are unchanged.
ALTER TABLE mm_grant
    ADD COLUMN session_id uuid REFERENCES play_session (id),
    ADD COLUMN network_ip inet,
    ADD COLUMN network_public_port integer,
    ADD COLUMN network_local_port integer,
    ADD COLUMN network_lan inet[],
    ADD COLUMN network_revision bigint,
    ADD COLUMN network_seen_at timestamptz,
    ADD CONSTRAINT mm_grant_network_shape CHECK (
        (network_ip IS NULL AND network_public_port IS NULL AND network_local_port IS NULL
            AND network_lan IS NULL AND network_revision IS NULL AND network_seen_at IS NULL)
        OR (session_id IS NOT NULL AND udp_proof_key IS NOT NULL
            AND network_ip IS NOT NULL AND network_public_port BETWEEN 1 AND 65535
            AND network_local_port BETWEEN 1 AND 65535 AND network_lan IS NOT NULL
            AND cardinality(network_lan) <= 4 AND network_revision BETWEEN 1 AND 4294967295
            AND network_seen_at IS NOT NULL
            AND network_public_port IS NOT NULL AND network_local_port IS NOT NULL
            AND network_revision IS NOT NULL)
    );

-- Every terminal transition clears addresses, including direct maintenance updates. Session
-- binding is immutable so a replacement session cannot take over another runtime's grant.
CREATE FUNCTION fi_mm_grant_network_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'UPDATE' AND NEW.session_id IS DISTINCT FROM OLD.session_id THEN
        RAISE EXCEPTION 'grant session binding is immutable';
    END IF;
    IF NEW.state NOT IN ('issued', 'used') THEN
        NEW.network_ip := NULL;
        NEW.network_public_port := NULL;
        NEW.network_local_port := NULL;
        NEW.network_lan := NULL;
        NEW.network_revision := NULL;
        NEW.network_seen_at := NULL;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER mm_grant_network_guard BEFORE INSERT OR UPDATE ON mm_grant
    FOR EACH ROW EXECUTE FUNCTION fi_mm_grant_network_guard();
