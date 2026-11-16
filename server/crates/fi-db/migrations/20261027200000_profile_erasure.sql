-- A player deletes one profile from the cloud, at once (owner 2026-10-27 "Players delete their own
-- profiles from the cloud, at once, 3 an hour"; ACCOUNTS.md §5 `DELETE /profiles/{xuid}` and §8
-- "Profile deletion"; register CR-S61). The account erasure (20261026190000_account_erasure.sql,
-- fi_db::erasure) and the route share one list of what a profile's erasure deletes: this function.
-- fi_db::erasure::erase_in calls it for each profile of an erased account (as fi_maintenance) and
-- fi_db::erasure::delete_profile_in for the one profile a player deletes (as fi_runtime).
--
-- fi_runtime never deletes save revisions, parked bundles, universe or progression rows
-- (20261017000100_foundation.sql, 20261019150000_mode_change.sql), so, as fi_purge_cloud_data, the
-- runtime only calls this function, owned by the migrator, and only for a profile already in the
-- state its callers create after their checks: the tombstone (`status = 'deleted'`, no name), with
-- no queued or active play session, no unexpired save lease and no open world it hosts. Returns
-- the rows per counter (fi_db::erasure's counter names; no names or ids).
--
-- Runs as the migrator (SECURITY DEFINER): `pg_temp` last in the search_path, every table
-- schema-qualified, as 20261019150000_mode_change.sql explains.
CREATE FUNCTION fi_erase_profile_data(p_profile uuid) RETURNS jsonb
LANGUAGE plpgsql VOLATILE SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE
  v_x bigint;
  n bigint;
  c jsonb := '{}'::jsonb;
BEGIN
  -- The runtime erases only a tombstone made in its own transaction (deleted_at at or after the
  -- transaction's start, `now()`): never an older one, so a runtime path cannot replay it over
  -- profiles erased before. The erasure job's login (a member of fi_maintenance) erases any
  -- tombstone of the account it erases.
  SELECT p.xuid INTO v_x FROM public.profile p
   WHERE p.id = p_profile AND p.status = 'deleted' AND p.name IS NULL
     AND (p.deleted_at >= now() OR pg_has_role(session_user, 'fi_maintenance', 'MEMBER'));
  IF NOT FOUND THEN
    RAISE EXCEPTION 'fi_erase_profile_data erases only a deleted profile (its tombstone)'
      USING ERRCODE = 'restrict_violation';
  END IF;
  IF EXISTS (SELECT 1 FROM public.play_session s WHERE s.profile_id = p_profile AND s.state IN ('queued', 'active'))
     OR EXISTS (SELECT 1 FROM public.save_head h WHERE h.profile_id = p_profile AND h.lease_expires_at > clock_timestamp())
     OR EXISTS (SELECT 1 FROM public.world w WHERE w.host_profile_id = p_profile AND w.closed_at IS NULL) THEN
    RAISE EXCEPTION 'fi_erase_profile_data: the profile is in use' USING ERRCODE = 'restrict_violation';
  END IF;

  DELETE FROM public.legacy_device_token t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('legacy_device_token', n);
  -- Records that name it (a created or changed profile's stored answer carries its name).
  DELETE FROM public.idempotency_record t WHERE t.resource_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('idempotency_record', n);
  -- Activity telemetry of its sessions (GATE_API.md §4a).
  DELETE FROM public.session_idle_observation t
   WHERE t.session_id IN (SELECT s.id FROM public.play_session s WHERE s.profile_id = p_profile);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('session_idle_observation', n);

  -- Social and matchmaking rows that name it, on both sides.
  DELETE FROM public.account_friend_request t WHERE t.asked_profile = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('account_friend_request', n);
  DELETE FROM public.world_invite t WHERE t.from_profile_id = p_profile
     OR t.grant_id IN (SELECT g.id FROM public.mm_grant g WHERE g.profile_id = p_profile);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('world_invite', n);
  DELETE FROM public.freeroam_ticket t WHERE t.owner_profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('freeroam_ticket', n);
  DELETE FROM public.freeroam_session t WHERE t.owner_profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('freeroam_session', n);
  DELETE FROM public.mm_grant t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('mm_grant', n);
  -- Its own closed worlds, anonymised (other players' grants, reports and crash reports name the
  -- row; a closed world's endpoint, code, cell and region are already cleared).
  UPDATE public.world t SET name = 'Erased', message = NULL, endpoint_lan = '{}',
         endpoint_local_port = NULL, peers = '{}',
         allow = CASE WHEN t.allow IS NULL THEN NULL ELSE '{}'::bigint[] END
   WHERE t.host_profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('world', n);
  -- Its XUID in other hosts' worlds.
  UPDATE public.world t SET
         peers = ARRAY(SELECT v FROM unnest(t.peers) v WHERE v <> v_x),
         allow = CASE WHEN t.allow IS NULL THEN NULL
                 ELSE ARRAY(SELECT v FROM unnest(t.allow) v WHERE v <> v_x) END
   WHERE t.host_profile_id <> p_profile AND (v_x = ANY (t.peers) OR v_x = ANY (t.allow));
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('world_peer', n);
  DELETE FROM public.world_ban t WHERE t.host_profile_id = p_profile OR t.target_profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('world_ban', n);

  -- The game's and the universe's rows.
  DELETE FROM public.player_position t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('player_position', n);
  DELETE FROM public.universe_refusal t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('universe_refusal', n);
  DELETE FROM public.game_score t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_score', n);
  DELETE FROM public.game_ghost t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_ghost', n);
  DELETE FROM public.game_xp t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_xp', n);
  DELETE FROM public.game_drivatar t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_drivatar', n);
  DELETE FROM public.game_profile t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_profile', n);
  DELETE FROM public.game_profile_table t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_profile_table', n);
  DELETE FROM public.game_player_presence t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_player_presence', n);
  DELETE FROM public.game_presence t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_presence', n);
  DELETE FROM public.game_inbox t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_inbox', n);
  DELETE FROM public.game_notification t WHERE t.profile_id = p_profile OR t.sender_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_notification', n);
  DELETE FROM public.game_livery t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_livery', n);
  DELETE FROM public.game_livery_draft t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_livery_draft', n);
  -- The design bytes (per profile, CR-S59): nothing names them once the two above are gone.
  DELETE FROM public.livery_blob t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('livery_blob', n);
  DELETE FROM public.game_logon t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_logon', n);
  DELETE FROM public.game_transaction t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('game_transaction', n);
  -- Its own assessments. A seed check recorded on another profile of the account naming it as the
  -- target (`seed_target`) is that profile's history and stays (fi_db::erasure::PROFILE_KEPT).
  DELETE FROM public.integrity_assessment t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('integrity_assessment', n);
  DELETE FROM public.integrity_baseline t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('integrity_baseline', n);
  DELETE FROM public.social_comment t WHERE t.author_profile_id = p_profile
     OR (t.target_kind = 'player' AND t.target_id = v_x::text);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('social_comment', n);
  -- Its participation in crash reports (the account's own reports are the account's).
  DELETE FROM public.crash_participant t WHERE t.xuid = v_x;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('crash_participant', n);
  -- Strikes stay (moderation history); their link to a parked bundle deleted below goes.
  UPDATE public.integrity_strike t SET parked_id = NULL
   WHERE t.parked_id IN (SELECT s.id FROM public.save_parked s WHERE s.profile_id = p_profile);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('integrity_strike', n);
  DELETE FROM public.progression_event t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('progression_event', n);
  -- Its progress projection (credits, XP, level, cars, races): the profile's numbers, gone with
  -- its facts (invariant I3 then holds: no facts, no row).
  DELETE FROM public.profile_progress t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('profile_progress', n);
  -- Rows an image older than CR-S55 wrote to the outbox naming it (goes with the table's drop).
  DELETE FROM public.outbox_event t
   WHERE t.envelope::text LIKE '%' || p_profile::text || '%' OR t.envelope::text LIKE '%' || v_x::text || '%';
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('outbox_event', n);

  -- The save line: no lease (an expired one may still be recorded), the head back to 0 (the guard
  -- allows it only for a deleted profile or a retired line), then its rows. A CR-S52 line seeded
  -- from another profile loses its provenance with its revisions, so that source seeds again. The
  -- objects are saves-gc's (CLOUD_SAVES.md §6).
  UPDATE public.save_head t SET lease_session_id = NULL, lease_token = NULL, lease_device_id = NULL,
         lease_device_label = NULL, lease_acquired_at = NULL, lease_expires_at = NULL,
         lease_ttl_ms = NULL, lease_accounted_at = NULL
   WHERE t.profile_id = p_profile AND t.lease_session_id IS NOT NULL;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('save_head_lease', n);
  -- Retired, so no launch, lease or commit lands on the tombstone's line afterwards (admission
  -- refuses a retired line; the head guard refuses a commit on it).
  UPDATE public.save_head t SET retired_at = clock_timestamp()
   WHERE t.profile_id = p_profile AND t.retired_at IS NULL;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('save_head_retired', n);
  UPDATE public.save_head t SET revision = 0, reset_revision = 0
   WHERE t.profile_id = p_profile AND (t.revision <> 0 OR t.reset_revision <> 0);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('save_head', n);
  DELETE FROM public.save_revision t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('save_revision', n);
  DELETE FROM public.save_parked t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('save_parked', n);
  DELETE FROM public.save_event t WHERE t.profile_id = p_profile;
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('save_event', n);
  RETURN c;
END
$$;

-- A function is executable by PUBLIC unless revoked.
REVOKE ALL ON FUNCTION fi_erase_profile_data(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION fi_erase_profile_data(uuid) TO fi_runtime, fi_maintenance;
