-- CR-S26/27/28: per-grant socket proof credentials and distinct public/local relay ports.
-- Existing grants intentionally remain NULL: unsupported legacy clients fail closed. Credentials
-- are generated only by the server when issuing a NEW grant, never by migration/backfill.
ALTER TABLE mm_grant
    ADD COLUMN udp_proof_key bytea,
    ADD CONSTRAINT mm_grant_udp_proof_key_length
        CHECK (udp_proof_key IS NULL OR octet_length(udp_proof_key) = 32);

-- A legacy world uses endpoint_port for both paths through COALESCE in authorized routing reads.
ALTER TABLE world
    ADD COLUMN endpoint_local_port integer,
    ADD CONSTRAINT world_endpoint_local_port_range
        CHECK (endpoint_local_port IS NULL OR endpoint_local_port BETWEEN 1 AND 65535);
