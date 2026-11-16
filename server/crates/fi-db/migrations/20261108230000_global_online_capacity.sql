-- OD-228 (GATE_API.md §2 "Global online capacity", CR-S108): one cap across every game, the most
-- active (admitted, non-dedicated) play sessions of all games at once (NULL unlimited). It applies
-- on top of each game's `online_capacity` (OD-71) and each pool's `capacity`: a launch is admitted
-- only under all three; the existing queue holds the rest. A singleton of its own, not a column of
-- `online_control` (OD-82): that row's triggers advance its revision and audit open/closed on every
-- update, and the runtime login only reads it.
--
-- NULL (unlimited) until set: the owner's value (100) is an operational step,
-- `fi-cli online-capacity --global --set 100`, never a migration's, so a deploy never overwrites
-- what an operator dialed in and the test databases' accumulated sessions are not capped.
--
-- Expand only. Instances of the previous build ignore the table until a rolling deploy replaces
-- them (they may admit past the cap for that minute).
CREATE TABLE global_online_capacity (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    capacity integer CHECK (capacity BETWEEN 1 AND 100000)
);
INSERT INTO global_online_capacity (capacity) VALUES (NULL);

-- Read by every admission (fi_runtime); set like `game.online_capacity` by the jobs login
-- (`fi-cli online-capacity --global`), which holds both roles' grants.
GRANT SELECT ON global_online_capacity TO fi_runtime, fi_maintenance;
GRANT UPDATE (capacity) ON global_online_capacity TO fi_runtime, fi_maintenance;
