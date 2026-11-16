-- A deleted profile's session history is kept 90 days after its deletion, then deleted (owner
-- 2026-10-27, "Manter por 90 dias, depois apagar", answering OPEN_QUESTIONS PD-Q1 / REVIEW_LOG
-- "CR-S61" item 3; ACCOUNTS.md §8 "Kept, pseudonymous"). The deletion instant is the tombstone's
-- `profile.deleted_at`: the player's deletion (CR-S61) and the account erasure both set it (the
-- `profile_deleted_when` check ties it to `status = 'deleted'`). The nightly `fi-cli data-retention`
-- job (`fi_db::retention::purge`, a fi_maintenance login) calls the function below in batches.
--
-- What references a session (every foreign key to play_session), and why none blocks the delete:
-- * the profile's own rows written in its sessions (game_presence, player_position, game_logon,
--   game_transaction, game_player_presence, game_drivatar, game_livery(_draft), progression_event,
--   profile_progress, mm_grant) are deleted at the profile's deletion (fi_erase_profile_data). A
--   row written for it after that (a repair path, a hand-made tombstone) would block the delete
--   and stop the nightly job for good, so the function applies the same rule to it: a nullable
--   reference is cleared, a row that only describes the session (its presence, its position) is
--   deleted, as the session it names goes;
-- * game_livery(_draft).session_id cascades (ON DELETE CASCADE, the profile's own designs);
-- * session_summary: the session's own counters, deleted with it (the same rule);
-- * world.host_session_id: a closed world the profile hosted stays (other players' grants, reports
--   and crash reports name the world), so its link to the session is cleared at the same time; a
--   world is closed before its host's profile can be deleted, and an open world still needs it
--   (world_open_has_host_session);
-- * crash_report.session_id: the account's own report, with its own expiry (CRASH_API.md); a
--   session a report still names waits for that report to expire.
--
-- fi_maintenance holds no DELETE on play_session or session_summary and no UPDATE grant is widened:
-- as fi_erase_profile_data, the job only calls this function, owned by the migrator (SECURITY
-- DEFINER, `pg_temp` last in the search_path, every table schema-qualified).

ALTER TABLE world ALTER COLUMN host_session_id DROP NOT NULL;
ALTER TABLE world ADD CONSTRAINT world_open_has_host_session
  CHECK (closed_at IS NOT NULL OR host_session_id IS NOT NULL);

-- The tombstones past the window, and every reference to a session by column: the function's
-- statements and the foreign-key checks of its delete (one lookup per deleted session and
-- referencing table; without an index, each is a scan of that table). Partial where the column is
-- nullable (`col = $1` implies NOT NULL). game_presence (its key), session_summary (its key),
-- progression_event and game_livery(_draft) already have one.
CREATE INDEX IF NOT EXISTS profile_deleted_at ON profile (deleted_at) WHERE status = 'deleted';
CREATE INDEX IF NOT EXISTS crash_report_session ON crash_report (session_id);
CREATE INDEX IF NOT EXISTS world_host_session ON world (host_session_id) WHERE host_session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS mm_grant_session ON mm_grant (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS player_position_session ON player_position (session_id);
CREATE INDEX IF NOT EXISTS game_logon_session ON game_logon (last_session_id) WHERE last_session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS game_transaction_session ON game_transaction (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS game_player_presence_session ON game_player_presence (session_id)
  WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS game_drivatar_session ON game_drivatar (session_id) WHERE session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS profile_progress_session ON profile_progress (last_session_id)
  WHERE last_session_id IS NOT NULL;

-- Deletes at most `p_batch` (1..10000) ended sessions of profiles deleted more than 90 days ago,
-- with their summaries, and clears them from the closed worlds those profiles hosted. Returns the
-- rows per counter (no ids). A rerun deletes nothing more.
CREATE FUNCTION fi_purge_deleted_profile_sessions(p_batch integer) RETURNS jsonb
LANGUAGE plpgsql VOLATILE SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE
  v_sessions uuid[];
  v_profiles uuid[];
  n bigint;
  v_refs bigint := 0;
  c jsonb := '{}'::jsonb;
BEGIN
  IF p_batch IS NULL OR p_batch < 1 OR p_batch > 10000 THEN
    RAISE EXCEPTION 'fi_purge_deleted_profile_sessions: batch must be 1..10000'
      USING ERRCODE = 'invalid_parameter_value';
  END IF;
  SELECT coalesce(array_agg(d.id), '{}'), coalesce(array_agg(DISTINCT d.profile_id), '{}')
    INTO v_sessions, v_profiles
    FROM (SELECT s.id, s.profile_id
            FROM public.profile p
            JOIN public.play_session s ON s.profile_id = p.id
           WHERE p.status = 'deleted'
             AND p.deleted_at < clock_timestamp() - make_interval(days => 90)
             AND s.state NOT IN ('queued', 'active')
             AND NOT EXISTS (SELECT 1 FROM public.crash_report r WHERE r.session_id = s.id)
           LIMIT p_batch
           FOR UPDATE OF s SKIP LOCKED) d;

  -- Leftovers that name the sessions (none after a normal deletion; see the header).
  UPDATE public.mm_grant t SET session_id = NULL WHERE t.session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  UPDATE public.game_logon t SET last_session_id = NULL WHERE t.last_session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  UPDATE public.game_transaction t SET session_id = NULL WHERE t.session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  UPDATE public.game_player_presence t SET session_id = NULL WHERE t.session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  UPDATE public.game_drivatar t SET session_id = NULL WHERE t.session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  UPDATE public.profile_progress t SET last_session_id = NULL WHERE t.last_session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  UPDATE public.progression_event t SET play_session_id = NULL WHERE t.play_session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  DELETE FROM public.game_presence t WHERE t.session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  DELETE FROM public.player_position t WHERE t.session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; v_refs := v_refs + n;
  c := c || jsonb_build_object('leftover', v_refs);

  DELETE FROM public.session_summary t WHERE t.play_session_id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('session_summary', n);
  UPDATE public.world t SET host_session_id = NULL
   WHERE t.host_profile_id = ANY (v_profiles) AND t.host_session_id = ANY (v_sessions)
     AND t.closed_at IS NOT NULL;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('world', n);
  DELETE FROM public.play_session t WHERE t.id = ANY (v_sessions);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('play_session', n);
  RETURN c;
END
$$;

REVOKE ALL ON FUNCTION fi_purge_deleted_profile_sessions(integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION fi_purge_deleted_profile_sessions(integer) TO fi_maintenance;
