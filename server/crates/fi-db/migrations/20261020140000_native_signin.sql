-- OD-74 / CR-S30: the launcher signs in natively with verified password ID tokens.
-- Retire short-lived browser/device credentials; identity tickets and launcher-to-site
-- portal codes remain. Historical migrations are immutable.
DROP TABLE launcher_auth_code;
DROP TABLE device_authorization;
