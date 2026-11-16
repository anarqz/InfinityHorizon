-- OD-52 (register CR-S20; docs/contracts/ACCOUNTS.md §5 "Mode change", SAVES_API.md §8 "Retired
-- line"): only a Supporter's Hardcore profile keeps data on the servers, so the one mode change
-- allowed, Hardcore -> Noob, deletes that data. The change is one transaction of
-- fi_db::purge::change_mode_in: the profile turns Noob, its save line is retired, and
-- fi_purge_cloud_data deletes its universe rows. Friendships are not cloud data of the profile:
-- they belong to the account and stay whatever the mode (owner, 2026-10-19).

-- 1. The retired line. A retired head never holds a lease (the CHECK: every grant, resume and
-- renewal sets lease_session_id), never takes a commit (the guard below: the revision never moves
-- forward), takes no parked bundle (the trigger on save_parked), and is never un-retired. Its
-- revisions and parked bundles are deleted by the retention job, not by the change: the guard lets
-- a retired head return to 0 (as it does for an erased profile) so the deferred foreign key lets
-- those rows go; the objects follow through the two-phase garbage collection (CLOUD_SAVES.md §6).
ALTER TABLE save_head
  ADD COLUMN retired_at timestamptz,
  ADD CONSTRAINT save_head_retired_has_no_lease CHECK (retired_at IS NULL OR lease_session_id IS NULL);

-- fi_save_head_guard of 20261017000400_saves.sql, plus the retired line's rules.
CREATE OR REPLACE FUNCTION fi_save_head_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.fence_seq < OLD.fence_seq THEN
    RAISE EXCEPTION 'save_head.fence_seq never goes back (% -> %)', OLD.fence_seq, NEW.fence_seq
      USING ERRCODE = 'restrict_violation';
  END IF;
  IF NEW.play_ms < OLD.play_ms THEN
    RAISE EXCEPTION 'save_head.play_ms never goes back' USING ERRCODE = 'restrict_violation';
  END IF;
  IF OLD.retired_at IS NOT NULL THEN
    IF NEW.retired_at IS DISTINCT FROM OLD.retired_at THEN
      RAISE EXCEPTION 'save_head.retired_at never changes once set' USING ERRCODE = 'restrict_violation';
    END IF;
    IF NEW.revision > OLD.revision THEN
      RAISE EXCEPTION 'a retired save line takes no commit' USING ERRCODE = 'restrict_violation';
    END IF;
  END IF;
  IF NEW.revision NOT IN (OLD.revision, OLD.revision + 1) OR NEW.reset_revision < OLD.reset_revision THEN
    IF NEW.revision = 0 AND NEW.reset_revision = 0
       AND (OLD.retired_at IS NOT NULL
            OR EXISTS (SELECT 1 FROM public.profile p WHERE p.id = NEW.profile_id AND p.status = 'deleted')) THEN
      RETURN NEW;
    END IF;
    RAISE EXCEPTION 'save_head moves to the next revision or stays (% -> %)', OLD.revision, NEW.revision
      USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

-- A park does not lock the head, so this reads the committed head: a park racing the change may
-- land on a just-retired line, where retention deletes it with the line's other rows.
CREATE FUNCTION fi_save_parked_line_live() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF EXISTS (SELECT 1 FROM public.save_head h WHERE h.profile_id = NEW.profile_id AND h.retired_at IS NOT NULL) THEN
    RAISE EXCEPTION 'a retired save line takes no parked bundle' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER save_parked_line_live BEFORE INSERT ON save_parked FOR EACH ROW EXECUTE FUNCTION fi_save_parked_line_live();

-- Retention zeroes a retired head (and an erased profile's) before it deletes the line's rows.
GRANT UPDATE (revision, reset_revision) ON save_head TO fi_maintenance;

-- 2. What the change deletes: one row per (table, column naming the profile). Every target has
-- a `game` column (ADR 0902 §1) and an index that serves `game = $1 AND <column> = $2` (leading
-- with (game, <column>), or with (game, mode, <column>) through PostgreSQL 18's skip scan over the
-- two modes), since both functions below filter on the two. `category` is the warning's
-- (ModeChangeWarning.deletes) under which the rows count; NULL rows are deleted but not shown.
-- Only the migrator writes this table.
--
-- To add a table (the inbox `game_notification`, clubs, creations): one line in its own migration,
--   INSERT INTO cloud_purge_target VALUES ('game_notification', 'profile_id', 'inbox');
-- and drop its row before the table is ever dropped.
--
-- Not targets, on purpose: `friendship` and `friend_request` (the account's, kept whatever the
-- mode, owner 2026-10-19), `game_logon` (the bounded logon state every tier keeps, OD-52),
-- `game_transaction` (the bounded Reconcile ring), `integrity_strike` (moderation history),
-- `progression_event` (the lake's append-only facts), `world_ban` (a host's bans either way),
-- `ghost_blob` and `save_blob` (shared objects, two-phase garbage collection), the save line
-- itself (retired above, deleted by retention).
CREATE TABLE cloud_purge_target (
  table_name  regclass NOT NULL,
  column_name name NOT NULL,
  category    text CHECK (category ~ '^[a-z][A-Za-z]{0,31}$'),
  PRIMARY KEY (table_name, column_name)
);

INSERT INTO cloud_purge_target (table_name, column_name, category) VALUES
  -- The game's own boards, ghosts, XP ledger and drivatars (OD-48; 20261019120000_scores.sql,
  -- 20261019120100_drivatars.sql).
  ('game_score', 'profile_id', 'scores'),
  ('game_ghost', 'profile_id', 'ghosts'),
  ('game_xp', 'profile_id', 'xp'),
  ('game_drivatar', 'profile_id', 'drivatars'),
  -- The platform universe of POST /saves/v1/progress (20261019000300_universe.sql).
  ('board_entry', 'profile_id', 'scores'),
  ('ghost', 'profile_id', 'ghosts'),
  ('player_position', 'profile_id', NULL),
  ('universe_refusal', 'profile_id', NULL),
  -- The profile sync, its gates' baseline and the Xbox Live presence friends see
  -- (20261019000400_gameplane.sql, 20261019120200_integrity.sql).
  ('game_profile', 'profile_id', 'onlineProfile'),
  ('game_profile_table', 'profile_id', NULL),
  ('integrity_baseline', 'profile_id', NULL),
  ('game_player_presence', 'profile_id', NULL);

-- The two targets whose keys do not lead with (game, profile_id) or (game, mode, profile_id).
CREATE INDEX board_entry_profile ON board_entry (game, profile_id);
CREATE INDEX ghost_profile ON ghost (game, profile_id);

-- 3 and 4 run as the migrator (SECURITY DEFINER), so nothing in them may resolve to a caller's
-- object: `pg_temp` comes last in their search_path (unlisted, PostgreSQL searches it FIRST, and
-- any login may create temporary tables named `profile` or `cloud_purge_target`), every table
-- they read is schema-qualified, and the dynamic statements name each target by its catalog
-- schema and name, quoted.

-- 3. The warning's counts, by category (every category present, zeros included).
CREATE FUNCTION fi_cloud_data_counts(p_game text, p_profile uuid) RETURNS jsonb
LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE
  t record;
  n bigint;
  counts jsonb := '{}'::jsonb;
BEGIN
  FOR t IN SELECT ns.nspname, c.relname, x.column_name, x.category
             FROM public.cloud_purge_target x
             JOIN pg_catalog.pg_class c ON c.oid = x.table_name
             JOIN pg_catalog.pg_namespace ns ON ns.oid = c.relnamespace
            WHERE x.category IS NOT NULL ORDER BY c.relname, x.column_name LOOP
    EXECUTE format('SELECT count(*) FROM %I.%I WHERE game = $1 AND %I = $2', t.nspname, t.relname, t.column_name)
      INTO n USING p_game, p_profile;
    counts := jsonb_set(counts, ARRAY[t.category], to_jsonb(COALESCE((counts ->> t.category)::bigint, 0) + n));
  END LOOP;
  RETURN counts;
END
$$;

-- 4. The purge. fi_runtime never deletes (20261017000100_foundation.sql), and granting it DELETE
-- on a dozen universe tables would let any runtime path delete any profile's boards. Instead the
-- runtime may only call this function, owned by the migrator, which deletes the rows of one
-- profile, and only of a profile that is Noob with a retired save line: the state only
-- fi_db::purge::change_mode_in creates, after its checks, in the transaction that calls this (the
-- runtime role could reach that state with two UPDATEs of its own, but never purge a Hardcore
-- profile through here). Returns the rows deleted per table (for the audit row; no names).
CREATE FUNCTION fi_purge_cloud_data(p_profile uuid) RETURNS jsonb
LANGUAGE plpgsql VOLATILE SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE
  v_game text;
  t record;
  n bigint;
  deleted jsonb := '{}'::jsonb;
BEGIN
  SELECT p.game INTO v_game
    FROM public.profile p JOIN public.save_head h ON h.profile_id = p.id
   WHERE p.id = p_profile AND p.mode = 'noob' AND h.retired_at IS NOT NULL;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'fi_purge_cloud_data purges only a Noob profile whose save line is retired'
      USING ERRCODE = 'restrict_violation';
  END IF;
  FOR t IN SELECT ns.nspname, c.relname, x.column_name
             FROM public.cloud_purge_target x
             JOIN pg_catalog.pg_class c ON c.oid = x.table_name
             JOIN pg_catalog.pg_namespace ns ON ns.oid = c.relnamespace
            ORDER BY c.relname, x.column_name LOOP
    EXECUTE format('DELETE FROM %I.%I WHERE game = $1 AND %I = $2', t.nspname, t.relname, t.column_name)
      USING v_game, p_profile;
    GET DIAGNOSTICS n = ROW_COUNT;
    deleted := jsonb_set(deleted, ARRAY[t.relname::text],
                         to_jsonb(COALESCE((deleted ->> t.relname::text)::bigint, 0) + n));
  END LOOP;
  RETURN deleted;
END
$$;

-- A function is executable by PUBLIC unless revoked; these two are the runtime's only.
REVOKE ALL ON FUNCTION fi_cloud_data_counts(text, uuid), fi_purge_cloud_data(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION fi_cloud_data_counts(text, uuid), fi_purge_cloud_data(uuid) TO fi_runtime;
