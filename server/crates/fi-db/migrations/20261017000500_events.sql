-- ForzaInfinity schema, part 5 of 5: the transactional outbox, the progression fact log and its
-- consolidated read models (docs/contracts/EVENTS.md, ADR 0007).
--
-- No service publishes to Pub/Sub without a durable row first: the business write and its
-- outbox_event row commit together. The api publishes its own rows right after commit (low
-- latency); the outbox sweeper job (fi-worker) publishes whatever is left, under a claim lease
-- (FOR UPDATE SKIP LOCKED), and prunes rows once BigQuery holds them (landed_at).

CREATE TABLE outbox_event (
  id                uuid PRIMARY KEY DEFAULT uuidv7(),
  event_id          uuid NOT NULL CONSTRAINT outbox_event_event_id_key UNIQUE,
  event_type        text NOT NULL CHECK (event_type ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$'),
  schema_version    integer NOT NULL CHECK (schema_version > 0),
  topic             text NOT NULL CHECK (topic ~ '^[a-z][a-z0-9-]*$'),
  account_id        uuid,
  entity_id         text NOT NULL CHECK (char_length(entity_id) BETWEEN 1 AND 128),
  entity_version    bigint CHECK (entity_version >= 0),
  ordering_key      text CHECK (char_length(ordering_key) <= 128),
  -- The complete envelope exactly as it will be published (validated by fi_events before insert).
  envelope          jsonb NOT NULL CHECK (jsonb_typeof(envelope) = 'object'),
  occurred_at       timestamptz NOT NULL,
  created_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
  attempts          integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  claimed_until     timestamptz,
  last_error        text CHECK (char_length(last_error) <= 2000),
  published_at      timestamptz,
  pubsub_message_id text,
  landed_at         timestamptz,
  CONSTRAINT outbox_event_published_shape CHECK ((published_at IS NULL) = (pubsub_message_id IS NULL)),
  CONSTRAINT outbox_event_landed_after_publish CHECK (landed_at IS NULL OR published_at IS NOT NULL)
);

-- Only the relay columns may change after insert.
CREATE FUNCTION fi_outbox_event_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF (to_jsonb(NEW) - ARRAY['attempts', 'claimed_until', 'last_error', 'published_at', 'pubsub_message_id', 'landed_at'])
     IS DISTINCT FROM
     (to_jsonb(OLD) - ARRAY['attempts', 'claimed_until', 'last_error', 'published_at', 'pubsub_message_id', 'landed_at']) THEN
    RAISE EXCEPTION 'outbox_event payload columns are immutable' USING ERRCODE = 'restrict_violation';
  END IF;
  IF OLD.published_at IS NOT NULL AND NEW.published_at IS DISTINCT FROM OLD.published_at THEN
    RAISE EXCEPTION 'a published outbox_event stays published' USING ERRCODE = 'restrict_violation';
  END IF;
  RETURN NEW;
END
$$;

CREATE TRIGGER outbox_event_immutable BEFORE UPDATE ON outbox_event FOR EACH ROW EXECUTE FUNCTION fi_outbox_event_guard();
CREATE INDEX outbox_event_unpublished ON outbox_event (created_at) WHERE published_at IS NULL;
CREATE INDEX outbox_event_unlanded ON outbox_event (published_at) WHERE published_at IS NOT NULL AND landed_at IS NULL;

-- Append-only facts from the game plane (profile syncs, scores, XP, race results), idempotent by
-- event id (derived from the profile and the hook's X-Fh2-Request-Id, so a retried request is the
-- same fact). The system of record for progression history; consolidated at session end and
-- nightly. Partitioned by month once it outgrows one table (ADR 0007 §6).
CREATE TABLE progression_event (
  event_id        uuid PRIMARY KEY,
  profile_id      uuid NOT NULL REFERENCES profile (id),
  play_session_id uuid REFERENCES play_session (id),
  kind            text NOT NULL CHECK (kind IN ('profile_sync', 'tables_sync', 'score', 'xp', 'race_result', 'drivatar', 'livery')),
  payload         jsonb NOT NULL CHECK (jsonb_typeof(payload) = 'object'),
  occurred_at     timestamptz NOT NULL,
  received_at     timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TRIGGER progression_event_append_only BEFORE UPDATE ON progression_event FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();
CREATE INDEX progression_event_profile ON progression_event (profile_id, received_at);
CREATE INDEX progression_event_session ON progression_event (play_session_id) WHERE play_session_id IS NOT NULL;

-- Consolidated per profile (session end and nightly): what the portal and the launcher show.
-- Two unordered consumers write it (save.committed and session.ended pushes, retried and
-- redelivered in any order), so the save-derived numbers carry the revision they come from and an
-- update applies only with a newer one: `... WHERE profile_progress.source_revision < $revision`
-- (save.committed's revision, session.ended's endRevision). Per-session sums (races, play_seconds)
-- are applied once per session through processed_event.
--
-- consolidated_through is a watermark over progression_event.received_at: every fact received
-- before it is applied. A fact's received_at is taken when its transaction inserts it, not when
-- that transaction commits, so a watermark at now() would skip a fact inserted earlier but
-- committed after the read (and so would an identity cursor). The watermark is therefore at most
-- now() - 60 s, and every writer of progression_event runs with transaction_timeout = 30 s
-- (fi_db::pool, service sessions): below the watermark, nothing can still be uncommitted.
CREATE TABLE profile_progress (
  profile_id           uuid PRIMARY KEY REFERENCES profile (id),
  credits              bigint,
  xp                   bigint,
  level                integer,
  cars                 integer,
  source_revision      bigint NOT NULL DEFAULT 0 CHECK (source_revision >= 0),
  races                integer NOT NULL DEFAULT 0 CHECK (races >= 0),
  play_seconds         bigint NOT NULL DEFAULT 0 CHECK (play_seconds >= 0),
  last_session_id      uuid REFERENCES play_session (id),
  consolidated_through timestamptz,
  created_at           timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at           timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version          bigint NOT NULL DEFAULT 1
);

CREATE TRIGGER profile_progress_touch BEFORE UPDATE ON profile_progress FOR EACH ROW EXECUTE FUNCTION fi_touch_row();

-- One row per ended session: the session-end consolidation's result. The nightly job builds the
-- row of every ended session that lacks one (a dead-lettered or lost session.ended push).
CREATE TABLE session_summary (
  play_session_id uuid PRIMARY KEY REFERENCES play_session (id),
  profile_id      uuid NOT NULL REFERENCES profile (id),
  started_at      timestamptz NOT NULL,
  ended_at        timestamptz NOT NULL,
  start_revision  bigint CHECK (start_revision >= 0),
  end_revision    bigint CHECK (end_revision >= 0),
  events          integer NOT NULL CHECK (events >= 0),
  xp_gained       bigint,
  credits_delta   bigint,
  races           integer NOT NULL DEFAULT 0 CHECK (races >= 0),
  consolidated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT session_summary_order CHECK (ended_at >= started_at)
);

CREATE INDEX session_summary_profile ON session_summary (profile_id, ended_at);

-- Push-subscription consumers record what they processed: Pub/Sub delivers at least once.
CREATE TABLE processed_event (
  consumer     text NOT NULL CHECK (consumer ~ '^[a-z][a-z0-9-]*$'),
  event_id     uuid NOT NULL,
  processed_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (consumer, event_id)
);

GRANT SELECT, INSERT ON outbox_event, progression_event, processed_event TO fi_runtime;
GRANT UPDATE (attempts, claimed_until, last_error, published_at, pubsub_message_id, landed_at) ON outbox_event TO fi_runtime;
GRANT SELECT, INSERT, UPDATE ON profile_progress, session_summary TO fi_runtime;
GRANT SELECT, DELETE ON outbox_event, processed_event TO fi_maintenance;
GRANT UPDATE (attempts, claimed_until, last_error, published_at, pubsub_message_id, landed_at) ON outbox_event TO fi_maintenance;
GRANT SELECT ON progression_event, profile_progress, session_summary TO fi_maintenance;
