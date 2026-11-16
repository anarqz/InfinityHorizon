-- NR-5, MATCHMAKING_API.md §3.9 "The per-car speed limit" (OD-163): a speed observation names the
-- observed car's ordinal when the hook knows it, and the speed held for the rules' sustain time;
-- the server judges it against that car's own limit (fi_db::observations::speed_limit_kmh).
--
-- Expand only: two nullable columns no older build reads or writes (an older build's INSERT names
-- its columns and leaves these NULL, which reads as "no car named": the flat limit).
ALTER TABLE peer_observation
  ADD COLUMN car_id integer CHECK (car_id IS NULL OR (car_id >= 0 AND car_id <= 1000000)),
  ADD COLUMN held_kmh double precision CHECK (held_kmh IS NULL OR (held_kmh > 0 AND held_kmh <= 5000));

-- §5d: a relay's speed report names the measured car when the relay decodes it (judged by
-- fi_db::relay_reports::judge with the same per-car limit).
ALTER TABLE relay_report
  ADD COLUMN car_id integer CHECK (car_id IS NULL OR (car_id >= 0 AND car_id <= 1000000));
