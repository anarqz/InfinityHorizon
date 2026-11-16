-- CR-S68 (CORE_API.md §4.4, §5, §5a; OD-154, OD-163): voice moderation.
--
-- `account.voice_banned_until`: a moderator's voice ban (`POST /core/v1/admin/voice-bans`).
-- `account.voice_community_until`: the end of the account's latest community mute (3 distinct
-- muter accounts within 7 days → 3 h); kept after it ends, since only mutes newer than it count.
-- An account's voice is blocked while the later of the two is in the future; it reaches the relays
-- through the relay grant's `voiceBlockedUntil` claim and `WorldMember.voiceBlockedUntil`.
ALTER TABLE account
  ADD COLUMN voice_banned_until timestamptz,
  ADD COLUMN voice_community_until timestamptz;

-- Every mute and unmute an account's launcher sent (`POST /launcher/v1/voice/mutes`): the history
-- moderators read and the community rule's input (an account's current state toward another is
-- its latest row). Ids only; never shown to the target. Kept 30 days (fi_db::retention).
CREATE TABLE voice_mute_event (
  id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  muter_account_id  uuid NOT NULL REFERENCES account (id),
  target_account_id uuid NOT NULL REFERENCES account (id),
  action            text NOT NULL CHECK (action IN ('muted', 'unmuted')),
  world_id          uuid NOT NULL REFERENCES world (id) ON DELETE CASCADE,
  at                timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT voice_mute_event_not_self CHECK (muter_account_id <> target_account_id)
);
CREATE INDEX voice_mute_event_pair ON voice_mute_event (muter_account_id, target_account_id, at DESC, id DESC);
CREATE INDEX voice_mute_event_target ON voice_mute_event (target_account_id, at DESC, id DESC);
CREATE INDEX voice_mute_event_world ON voice_mute_event (world_id);
CREATE INDEX voice_mute_event_at ON voice_mute_event (at);

GRANT SELECT, INSERT ON voice_mute_event TO fi_runtime;
-- Erasure (fi_db::erasure::STEPS) as fi_runtime and fi_maintenance; retention as fi_maintenance.
GRANT DELETE ON voice_mute_event TO fi_runtime;
GRANT SELECT, DELETE ON voice_mute_event TO fi_maintenance;

-- The staff feed rows of voice moderation (#moderation-chat): expand only (the previous build
-- writes none of them; its bot ignores kinds it does not know).
ALTER TABLE public_event DROP CONSTRAINT public_event_kind_check;
ALTER TABLE public_event
  ADD CONSTRAINT public_event_kind_check CHECK (kind IN ('player_joined', 'lobby_opened',
    'player_kicked', 'player_banned', 'speed_record', 'route_record',
    'voice_community_muted', 'voice_banned', 'voice_unbanned')) NOT VALID;
ALTER TABLE public_event VALIDATE CONSTRAINT public_event_kind_check;
ALTER TABLE public_event DROP CONSTRAINT public_event_audience_of_kind;
ALTER TABLE public_event
  ADD CONSTRAINT public_event_audience_of_kind CHECK ((audience = 'staff') = (kind IN ('player_kicked',
    'player_banned', 'voice_community_muted', 'voice_banned', 'voice_unbanned'))) NOT VALID;
ALTER TABLE public_event VALIDATE CONSTRAINT public_event_audience_of_kind;
