-- NR-5, MATCHMAKING_API.md §5d step 3: a relay speed report the flat rules support about a car
-- whose own per-car limit is above the flat one is kept `recorded` with reason `car_limit`, never
-- flagged (fi_db::relay_reports::over_car_limit).
--
-- Expand only: one more allowed reason; no older build writes it.
ALTER TABLE relay_report DROP CONSTRAINT relay_report_reason_check;
ALTER TABLE relay_report ADD CONSTRAINT relay_report_reason_check CHECK (reason IN ('world', 'relay',
  'member', 'host', 'stale', 'rules', 'transport', 'scope', 'jump', 'dedicated', 'car_limit'));
