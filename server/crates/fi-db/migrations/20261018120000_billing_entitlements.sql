-- ForzaInfinity schema: monetization's entitlement tables (MON-1.2; ADR 0301, ADR 0305 §3 as
-- amended 2026-10-17). Schema owned by monetization; written only by fi-billing code paths
-- (services/billing `store`, called by its admin CLI and tick job, later its webhooks and
-- reconcile). Read by the account plane under the ACCOUNTS.md §7 rule, as a plain SELECT
-- (fi_billing::store::read_access).
--
--   entitlement        the cached state per (account, product): status, window, cache of the
--                      fi_entitlements::Entitlement (`state`, disposable: rebuilt by replay)
--   entitlement_event  the ledger, append-only: every applied or ignored billing event and every
--                      materialized tick, in `seq` order; `event_id UNIQUE` is the idempotency
--                      key (a duplicate is not a row)
--   admin_action       the operator audit, append-only
--
-- billing_webhook_inbox, billing_link and billing_settings come with MON-M3's own migration file.

CREATE TABLE entitlement (
  account_id   uuid NOT NULL REFERENCES account (id),
  product      text NOT NULL DEFAULT 'vip' CHECK (product = 'vip'),
  -- fi_entitlements::Status as last materialized (what entitlement.changed last said).
  status       text NOT NULL CHECK (status IN ('none', 'active', 'grace', 'suspended', 'expired', 'revoked_refund', 'revoked_chargeback', 'revoked_admin')),
  -- Plan and provider of the coverage (Entitlement::current_plan / current_provider).
  plan         text CHECK (plan IN ('monthly', 'yearly', 'pass', 'comp')),
  provider     text CHECK (provider IN ('patreon', 'kofi', 'stripe', 'mercadopago', 'admin', 'reconcile')),
  -- Furthest valid paid period or comp end; access_until adds grace and is NULL when revoked,
  -- blocked or suspended. Both are what the state machine says regardless of `now`: readers
  -- apply `access_until > now()` (fi_domain::routing::Access::tier_at).
  coverage_end timestamptz,
  access_until timestamptz,
  auto_renew   boolean NOT NULL,
  past_due     boolean NOT NULL,
  blocked      boolean NOT NULL,
  -- Entitlement::version: bumped by every applied change and every materialized tick; the
  -- entityVersion of entitlement.changed.
  version      bigint NOT NULL CHECK (version >= 0),
  -- serde form of fi_entitlements::Entitlement ({"schema": 1, ...}); NULL or unreadable = replay.
  state        jsonb CHECK (state IS NULL OR jsonb_typeof(state) = 'object'),
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  updated_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  row_version  bigint NOT NULL DEFAULT 1,
  PRIMARY KEY (account_id, product),
  -- A VIP-granting row always has an end: a NULL would silently read as free (tier_at), and an
  -- unbounded grant must never exist.
  CONSTRAINT entitlement_vip_is_bounded CHECK (status NOT IN ('active', 'grace') OR access_until IS NOT NULL)
);

CREATE TRIGGER entitlement_touch BEFORE UPDATE ON entitlement FOR EACH ROW EXECUTE FUNCTION fi_touch_row();
-- The tick job's candidates (fi_billing::store::tick_due).
CREATE INDEX entitlement_vip_until ON entitlement (access_until) WHERE status IN ('active', 'grace');
CREATE INDEX entitlement_active_coverage ON entitlement (coverage_end) WHERE status = 'active';

CREATE TABLE entitlement_event (
  id            uuid PRIMARY KEY DEFAULT uuidv7(),
  account_id    uuid NOT NULL,
  product       text NOT NULL,
  -- Application order per entitlement: replay reads ORDER BY seq.
  seq           bigint NOT NULL CHECK (seq >= 1),
  -- fi_entitlements::EventId ("<provider>:<id>"), or 'tick:<account>:<version>' for a tick.
  event_id      text NOT NULL CONSTRAINT entitlement_event_event_id_key UNIQUE CHECK (char_length(event_id) BETWEEN 3 AND 200),
  provider      text CHECK (provider IN ('patreon', 'kofi', 'stripe', 'mercadopago', 'admin', 'reconcile')),
  kind          text NOT NULL CHECK (kind IN ('payment_succeeded', 'payment_failed', 'renewal_canceled', 'renewal_resumed', 'subscription_ended', 'refunded', 'dispute_opened', 'dispute_closed', 'admin_grant', 'admin_revoke', 'admin_restore', 'tick')),
  -- The provider's instant (BillingEvent::occurred_at); the tick instant for a tick.
  occurred_at   timestamptz NOT NULL,
  -- The exact `now` the state machine applied it at (milliseconds): replay is bit-exact.
  applied_at    timestamptz NOT NULL,
  -- The ledger JSON (serde of fi_entitlements::BillingEvent; {"type": "tick"} for a tick).
  event         jsonb NOT NULL CHECK (jsonb_typeof(event) = 'object'),
  outcome       text NOT NULL CHECK (outcome IN ('applied', 'ignored', 'tick')),
  ignore_reason text CHECK (ignore_reason IN ('no_change', 'other_subscription', 'stale_renewal_flag', 'subscription_ended')),
  from_status   text NOT NULL CHECK (from_status IN ('none', 'active', 'grace', 'suspended', 'expired', 'revoked_refund', 'revoked_chargeback', 'revoked_admin')),
  to_status     text NOT NULL CHECK (to_status IN ('none', 'active', 'grace', 'suspended', 'expired', 'revoked_refund', 'revoked_chargeback', 'revoked_admin')),
  -- Entitlement version after this row.
  version       bigint NOT NULL CHECK (version >= 0),
  -- fi_events actor form (EVENTS.md §1), restricted to the billing writers.
  actor         text NOT NULL CHECK (actor ~ '^(billing|admin|job):[A-Za-z0-9._@-]{1,128}$'),
  recorded_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT entitlement_event_seq_key UNIQUE (account_id, product, seq),
  CONSTRAINT entitlement_event_entitlement_fk FOREIGN KEY (account_id, product) REFERENCES entitlement (account_id, product),
  CONSTRAINT entitlement_event_tick_shape CHECK ((kind = 'tick') = (provider IS NULL) AND (kind = 'tick') = (outcome = 'tick')),
  CONSTRAINT entitlement_event_provider_prefix CHECK (kind = 'tick' OR provider = split_part(event_id, ':', 1)),
  CONSTRAINT entitlement_event_ignore_shape CHECK ((outcome = 'ignored') = (ignore_reason IS NOT NULL))
);

-- One version step per applied change or tick: a lost update (two writers applying on the same
-- version) fails here even if the row lock were ever skipped.
CREATE UNIQUE INDEX entitlement_event_version_key ON entitlement_event (account_id, product, version) WHERE outcome IN ('applied', 'tick');
-- Append-only, no exceptions yet (pseudonymisation at erasure, ADR 0305 §8, comes with its own
-- migration: docs/monetization/REVIEW_LOG.md).
CREATE TRIGGER entitlement_event_append_only BEFORE UPDATE OR DELETE ON entitlement_event FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();

CREATE TABLE admin_action (
  id         uuid PRIMARY KEY DEFAULT uuidv7(),
  actor      text NOT NULL CHECK (actor ~ '^admin:[A-Za-z0-9._@-]{1,128}$'),
  action     text NOT NULL CHECK (action ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$'),
  account_id uuid REFERENCES account (id),
  -- Operator text, kept forever: bounded, no control characters (checked by fi-billing too).
  reason     text NOT NULL CHECK (char_length(reason) BETWEEN 1 AND 500 AND reason !~ '[[:cntrl:]]'),
  at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  detail     jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (jsonb_typeof(detail) = 'object')
);

CREATE TRIGGER admin_action_append_only BEFORE UPDATE OR DELETE ON admin_action FOR EACH ROW EXECUTE FUNCTION fi_forbid_update();
CREATE INDEX admin_action_account ON admin_action (account_id, at) WHERE account_id IS NOT NULL;

-- No DELETE for anyone; the runtime role never updates the ledger or the audit.
GRANT SELECT, INSERT, UPDATE ON entitlement TO fi_runtime;
GRANT SELECT, INSERT ON entitlement_event, admin_action TO fi_runtime;
GRANT SELECT ON entitlement, entitlement_event, admin_action TO fi_maintenance;
