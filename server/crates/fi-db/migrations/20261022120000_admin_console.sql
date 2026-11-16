-- Local admin console (apps/admin-console): account contact/role columns, a registration switch
-- the services read next to FI_REGISTRATION_ENABLED, and the grants that let the console's
-- fi_maintenance login change capacity and audit what it did.
ALTER TABLE account ADD COLUMN discord varchar(255);

CREATE TABLE registration_control (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    open boolean NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
INSERT INTO registration_control (open) VALUES (true);

GRANT SELECT ON registration_control TO fi_runtime, fi_maintenance;
GRANT UPDATE ON registration_control TO fi_maintenance;
GRANT UPDATE (online_capacity) ON game TO fi_maintenance;
GRANT INSERT ON admin_action TO fi_maintenance;
