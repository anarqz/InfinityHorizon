-- CR-S98 (OD-206, OD-207; docs/contracts/CAREERS.md): a player-facing profile above the per-game
-- rows. `player_profile` holds the gamertag, the XUID and the mode; today's `profile` row becomes a
-- career, one per (profile, game), keyed by its own id as before, so every table, ticket and query
-- keyed on it is unchanged ("Parent above rows", owner 2026-11-07).
--
-- * Backfill: every existing `profile` row becomes its own profile with the same id, account,
--   mode, name, XUID and status, so a profile's id equals its FH2 career's id for every profile
--   that exists today. Existing accounts keep up to three profiles of one mode: the one-per-mode
--   limit is the creation's (fi_db::accounts::add_profile_in), the cleanup a separate step
--   (`fi-cli profiles duplicates` reports it, read-only).
-- * XUID: unique per profile, and per (game, XUID) among live careers (it was unique per career).
-- * Gamertag: unique among profiles that are not erased, case-insensitive, in every game (it was
--   per game; every existing profile is FH2's, so the index is the same set of rows).
-- * A career's account, mode, name and XUID equal its profile's while the career lives (the
--   triggers below, both ways). A career's deletion (CR-S61) tombstones only the career: its name
--   goes, its profile keeps the gamertag and the XUID (CAREERS.md §3 "delete career").
-- * A career inserted without `player_profile_id` (imports, seeds, the suites' fixtures) joins the
--   account's profile of its XUID, else becomes a new profile with its own id (the backfill's rule
--   for every row written later by code that predates CR-S98).

CREATE TABLE player_profile (
  id          uuid PRIMARY KEY DEFAULT uuidv7(),
  account_id  uuid NOT NULL REFERENCES account (id),
  mode        text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  -- The plain gamertag (OD-120); NULL once the profile is erased.
  name        text CHECK (name ~ '^[A-Za-z][A-Za-z0-9]{0,19}$'),
  xuid        bigint NOT NULL CONSTRAINT player_profile_xuid_key UNIQUE CHECK (xuid > 2535000000000000),
  status      text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'deletion_pending', 'deleted')),
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version bigint NOT NULL DEFAULT 1,
  deleted_at  timestamptz,
  CONSTRAINT player_profile_named_unless_deleted CHECK ((status = 'deleted') = (name IS NULL)),
  CONSTRAINT player_profile_deleted_when CHECK ((status = 'deleted') = (deleted_at IS NOT NULL))
);

CREATE TRIGGER player_profile_touch BEFORE UPDATE ON player_profile FOR EACH ROW EXECUTE FUNCTION fi_touch_row();
CREATE INDEX player_profile_account ON player_profile (account_id);

INSERT INTO player_profile (id, account_id, mode, name, xuid, status, created_at, deleted_at)
SELECT id, account_id, mode, name, xuid, status, created_at, deleted_at FROM profile;

-- Global, among profiles not erased (an erased tombstone has no name).
CREATE UNIQUE INDEX player_profile_name_key ON player_profile (lower(name)) WHERE name IS NOT NULL;

ALTER TABLE profile ADD COLUMN player_profile_id uuid REFERENCES player_profile (id);
UPDATE profile SET player_profile_id = id;
ALTER TABLE profile ALTER COLUMN player_profile_id SET NOT NULL;

ALTER TABLE profile DROP CONSTRAINT profile_xuid_key;
-- Among live careers: a deleted career stays a tombstone, and its game can start again under the
-- same profile and XUID (CAREERS.md §3 "delete career").
CREATE UNIQUE INDEX profile_game_xuid_key ON profile (game, xuid) WHERE status <> 'deleted';
CREATE UNIQUE INDEX profile_one_career_per_game ON profile (player_profile_id, game)
  WHERE status <> 'deleted';
-- Lookups by XUID alone (moderation, the card, crash participants) keep an index.
CREATE INDEX profile_xuid ON profile (xuid);

-- A career takes its profile's account, mode, name and XUID; one inserted without a profile joins
-- the account's profile of its XUID or becomes its own (see the header).
CREATE FUNCTION fi_career_insert() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
  pp record;
BEGIN
  IF NEW.player_profile_id IS NULL THEN
    SELECT p.id INTO NEW.player_profile_id FROM player_profile p
     WHERE p.xuid = NEW.xuid AND p.account_id = NEW.account_id;
    IF NOT FOUND THEN
      INSERT INTO player_profile (id, account_id, mode, name, xuid, status, created_at, deleted_at)
      VALUES (NEW.id, NEW.account_id, NEW.mode, NEW.name, NEW.xuid, NEW.status, NEW.created_at,
              NEW.deleted_at);
      NEW.player_profile_id := NEW.id;
      RETURN NEW;
    END IF;
  END IF;
  IF NEW.status <> 'deleted' THEN
    SELECT p.account_id, p.mode, p.name, p.xuid INTO pp FROM player_profile p
     WHERE p.id = NEW.player_profile_id;
    IF FOUND THEN
      NEW.account_id := pp.account_id;
      NEW.mode := pp.mode;
      NEW.name := pp.name;
      NEW.xuid := pp.xuid;
    END IF;
  END IF;
  RETURN NEW;
END $$;

CREATE TRIGGER profile_career_insert BEFORE INSERT ON profile FOR EACH ROW EXECUTE FUNCTION fi_career_insert();

-- A live career's change of name, mode, XUID or account is its profile's (a rename, the mode
-- change, a repair); the profile's change reaches its other live careers through the trigger after
-- it. Both stop once the rows are equal, so the second hop writes nothing.
CREATE FUNCTION fi_career_to_profile() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.status <> 'deleted' THEN
    UPDATE player_profile p
       SET account_id = NEW.account_id, mode = NEW.mode, name = NEW.name, xuid = NEW.xuid
     WHERE p.id = NEW.player_profile_id AND p.status <> 'deleted'
       AND (p.account_id, p.mode, p.name, p.xuid)
           IS DISTINCT FROM (NEW.account_id, NEW.mode, NEW.name, NEW.xuid);
  END IF;
  RETURN NULL;
END $$;

CREATE TRIGGER profile_career_sync AFTER UPDATE OF account_id, mode, name, xuid ON profile
  FOR EACH ROW EXECUTE FUNCTION fi_career_to_profile();

CREATE FUNCTION fi_profile_to_careers() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.status <> 'deleted' THEN
    UPDATE profile c
       SET account_id = NEW.account_id, mode = NEW.mode, name = NEW.name, xuid = NEW.xuid
     WHERE c.player_profile_id = NEW.id AND c.status <> 'deleted'
       AND (c.account_id, c.mode, c.name, c.xuid)
           IS DISTINCT FROM (NEW.account_id, NEW.mode, NEW.name, NEW.xuid);
  END IF;
  RETURN NULL;
END $$;

CREATE TRIGGER player_profile_careers_sync AFTER UPDATE OF account_id, mode, name, xuid ON player_profile
  FOR EACH ROW EXECUTE FUNCTION fi_profile_to_careers();

-- No login deletes careers (an erased one stays a tombstone, its XUID never reused); a row deleted
-- by the owner (a repair, the suites' cleanup) takes its profile with it once no career names it,
-- so no profile outlives its rows holding a name.
CREATE FUNCTION fi_career_deleted() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  DELETE FROM player_profile p
   WHERE p.id = OLD.player_profile_id
     AND NOT EXISTS (SELECT 1 FROM profile c WHERE c.player_profile_id = OLD.player_profile_id);
  RETURN NULL;
END $$;

CREATE TRIGGER profile_career_deleted AFTER DELETE ON profile FOR EACH ROW EXECUTE FUNCTION fi_career_deleted();

-- As `profile` (20261017000200_accounts.sql): the runtime creates, renames and deletes profiles,
-- the erasure job (fi_maintenance) tombstones them.
GRANT SELECT, INSERT, UPDATE ON player_profile TO fi_runtime;
GRANT SELECT, UPDATE ON player_profile TO fi_maintenance;

-- CR-S98: one career's erasure leaves its sibling careers' rows. The per-profile list of
-- 20261027200000_profile_erasure.sql, with three changes: other hosts' worlds forget the XUID in this
-- career's game only; comments on the player's page go in this career's game only; crash
-- participations and outbox rows that name the XUID alone go only once no other career of the XUID
-- is live (the last career of a deleted profile). Same signature, owner and grants.
CREATE OR REPLACE FUNCTION fi_erase_profile_data(p_profile uuid) RETURNS jsonb
LANGUAGE plpgsql VOLATILE SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp AS $$
DECLARE
  v_x bigint;
  v_game text;
  v_shared boolean;
  n bigint;
  c jsonb := '{}'::jsonb;
BEGIN
  -- The runtime erases only a tombstone made in its own transaction (deleted_at at or after the
  -- transaction's start, `now()`): never an older one, so a runtime path cannot replay it over
  -- profiles erased before. The erasure job's login (a member of fi_maintenance) erases any
  -- tombstone of the account it erases.
  SELECT p.xuid, p.game INTO v_x, v_game FROM public.profile p
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
  -- CR-S98: the XUID is also every other career's of the profile. While one stays live, rows keyed
  -- by the XUID alone stay (they are that career's too); rows of a game are this game's only.
  v_shared := EXISTS (SELECT 1 FROM public.profile o
                       WHERE o.xuid = v_x AND o.id <> p_profile AND o.status <> 'deleted');

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
   WHERE t.host_profile_id <> p_profile AND t.game = v_game
     AND (v_x = ANY (t.peers) OR v_x = ANY (t.allow));
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
     OR (t.target_kind = 'player' AND t.target_id = v_x::text AND t.game = v_game);
  GET DIAGNOSTICS n = ROW_COUNT; c := c || jsonb_build_object('social_comment', n);
  -- Its participation in crash reports (the account's own reports are the account's).
  DELETE FROM public.crash_participant t WHERE t.xuid = v_x AND NOT v_shared;
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
   WHERE t.envelope::text LIKE '%' || p_profile::text || '%' OR (NOT v_shared AND t.envelope::text LIKE '%' || v_x::text || '%');
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
