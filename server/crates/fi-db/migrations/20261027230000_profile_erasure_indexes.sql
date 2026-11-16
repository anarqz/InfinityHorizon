-- Indexes for the per-profile statements of fi_erase_profile_data (20261027200000_profile_erasure.sql;
-- docs/server/REVIEW_LOG.md "CR-S61" item 5). Each statement below filtered a table on a column no
-- index could serve, so the function scanned the whole table under the account's lock, once per
-- profile of an erased account and on every player's profile deletion.
--
-- Not added, because an existing index already serves the predicate:
-- * `(game, profile_id, ...)` keys (game_profile, game_inbox, game_logon, game_score, game_ghost,
--   game_xp, game_drivatar, game_transaction, game_profile_table, game_player_presence,
--   player_position, universe_refusal, integrity_assessment, integrity_baseline, freeroam_*,
--   world_ban, game_livery, game_livery_draft, game_notification's inbox): Postgres 18 skip scan
--   reads them for `profile_id = $1` over the few `game` (and `mode`) values;
-- * legacy_device_token, play_session, crash_participant, integrity_strike, livery_blob,
--   progression_event, profile_progress, save_* and session_idle_observation lead with the column.
--
-- Left as scans (REVIEW_LOG "CR-S61" item 5): `v_x = ANY (world.peers / allow)` (an array
-- membership no btree serves and GIN serves only as `@>`), the world_invite OR below and
-- `outbox_event.envelope::text LIKE` (a table no image writes since CR-S55, dropped with it).
--
-- In a transaction (plain CREATE INDEX): the production tables hold no player data (OD-103), so the
-- write lock is momentary. IF NOT EXISTS keeps a rerun on a hand-patched database harmless.

-- world: `host_profile_id = $1` (anonymise the profile's own worlds). The two existing indexes are
-- complementary partials (open / closed) the planner does not combine.
CREATE INDEX IF NOT EXISTS world_host_profile ON world (host_profile_id);

-- mm_grant: `profile_id = $1` (its seats), also the world_invite subquery below. The existing
-- `(game, profile_id)` indexes are partials over some states only.
CREATE INDEX IF NOT EXISTS mm_grant_profile ON mm_grant (profile_id);

-- world_invite: `from_profile_id = $1 OR grant_id IN (subquery)` itself stays a scan (an OR with a
-- hashed subplan; REVIEW_LOG "CR-S61" item 5), but the next statement's delete of the profile's
-- mm_grant rows checks the foreign key world_invite.grant_id row by row, which this index serves.
CREATE INDEX IF NOT EXISTS world_invite_grant ON world_invite (grant_id) WHERE grant_id IS NOT NULL;

-- account_friend_request: `asked_profile = $1`.
CREATE INDEX IF NOT EXISTS account_friend_request_asked_profile ON account_friend_request (asked_profile);

-- idempotency_record: `resource_id = $1` (records that name the profile).
CREATE INDEX IF NOT EXISTS idempotency_record_resource ON idempotency_record (resource_id)
  WHERE resource_id IS NOT NULL;

-- game_presence: `profile_id = $1` (keyed by session).
CREATE INDEX IF NOT EXISTS game_presence_profile ON game_presence (profile_id);

-- game_notification: `sender_id = $1` (the existing sender index covers type 512 only).
CREATE INDEX IF NOT EXISTS game_notification_sent_by ON game_notification (sender_id)
  WHERE sender_id IS NOT NULL;

-- social_comment: `author_profile_id = $1 OR (target_kind = 'player' AND target_id = $xuid)` (the
-- existing target index skips deleted and hidden comments, which the erasure deletes too).
CREATE INDEX IF NOT EXISTS social_comment_author_profile ON social_comment (author_profile_id);
CREATE INDEX IF NOT EXISTS social_comment_player_target ON social_comment (target_id)
  WHERE target_kind = 'player';
