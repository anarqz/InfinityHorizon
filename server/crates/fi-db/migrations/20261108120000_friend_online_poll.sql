-- Friends coming online (OD-211, register CR-S102; CORE_API.md §4.2a): `GET /launcher/v1/invites`
-- names the caller's friends whose account had a play session admitted since this device's
-- previous poll. One row per account and device binding: the time of the last poll. A row older
-- than an hour no longer counts (the next poll answers nothing and rewrites it) and is dropped by
-- the account's next poll from another device; the account's erasure deletes them all
-- (fi_db::erasure::STEPS). No addresses, no payloads.
--
-- Expand only: a new table the previous build never reads.
CREATE TABLE friend_online_poll (
  account_id        uuid NOT NULL REFERENCES account (id),
  device_binding_id uuid NOT NULL REFERENCES device_binding (id),
  polled_at         timestamptz NOT NULL,
  PRIMARY KEY (account_id, device_binding_id)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON friend_online_poll TO fi_runtime;
GRANT SELECT, DELETE ON friend_online_poll TO fi_maintenance;
