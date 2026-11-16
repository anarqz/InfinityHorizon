-- Car Clubs (SER-M3 step 8; OD-129 everyone, OD-130 "Clubs should also be split in modes";
-- docs/contracts/GAME_PLATFORM.md §3a "Car Clubs", register CR-S70): the game's
-- Forza.WebServices.CarClub and Xls.WebServices.Club methods, ported from the Beta's SQLite
-- `clubs`, `club_members`, `club_invites`, `club_requests`, `club_history`, `club_notices` and
-- `user_settings` (deprecated/beta/server/src/db/clubs.rs), keyed by game, mode and profile
-- instead of the XUID. Written and read by fi_db::clubs on the request's one transaction. Nothing of
-- the Beta is imported (OD-48).
--
-- A club belongs to one mode (`game_club.mode`): its members, invites, requests, notice board,
-- ladder and boards are of that mode only, and a reader of the other mode never sees it. A profile
-- is in at most one club (`game_club_member` is keyed by the profile); a mode change leaves it
-- (fi_db::purge::change_mode_in calls fi_db::clubs::leave_mode_in before the purge), an erasure
-- too (fi_db::erasure, fi_db::clubs::erase_profile_in). Every club write of a game and mode runs
-- under one transaction-scoped advisory lock (fi_db::clubs::lock_in), as the Beta's single SQLite
-- writer did: one club per player, succession and the empty club's deletion are decided one write
-- at a time.
--
-- Unlike the scoreboard lane, the runtime deletes here: leaving a club, the last member out
-- deleting it (its invites, requests and notices go with it, ON DELETE CASCADE), an invite or a
-- request answered, a notice taken down, the bounded notice board and history pruned.

-- 1. `progression_event` takes the `club` facts of the club writes (the request's fact id, so a
-- replayed request writes nothing): every other kind as before.
ALTER TABLE progression_event DROP CONSTRAINT progression_event_kind_check;
ALTER TABLE progression_event ADD CONSTRAINT progression_event_kind_check
  CHECK (kind IN ('profile_sync', 'tables_sync', 'score', 'xp', 'race_result', 'drivatar', 'livery', 'ghost', 'club'))
  NOT VALID;
ALTER TABLE progression_event VALIDATE CONSTRAINT progression_event_kind_check;

-- 2. The clubs. `id` is the wire's clubId (an i32; 0 means "no club", so the identity starts at 1
-- and stops at the i32 maximum). The tag is stored in capitals (2 to 4 letters or digits), unique
-- per game and mode: case-insensitive, as the Beta's `COLLATE NOCASE`. Keyword ids are the game's
-- u8 (0 = none).
CREATE TABLE game_club (
  game        text NOT NULL REFERENCES game (code),
  id          integer GENERATED ALWAYS AS IDENTITY (MAXVALUE 2147483647) NOT NULL,
  mode        text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  tag         text NOT NULL CHECK (tag ~ '^[A-Z0-9]{2,4}$'),
  name        text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 24),
  description text NOT NULL DEFAULT '' CHECK (char_length(description) <= 200),
  -- 1 Open, 2 OpenToFriends, 3 InviteOnly, 4 Private (the game's numbering).
  privacy     smallint NOT NULL CHECK (privacy BETWEEN 1 AND 4),
  region      smallint NOT NULL DEFAULT 0 CHECK (region BETWEEN 0 AND 255),
  attitude    smallint NOT NULL DEFAULT 0 CHECK (attitude BETWEEN 0 AND 255),
  interest    smallint NOT NULL DEFAULT 0 CHECK (interest BETWEEN 0 AND 255),
  created_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, id)
);

CREATE UNIQUE INDEX game_club_tag ON game_club (game, mode, tag);
CREATE INDEX game_club_mode ON game_club (game, mode, id);

-- 3. Members: one row per profile (one club per player). `role` 1 Leader, 2 CoLeader, 3 Member;
-- one leader per club. `xp` is what the member earned while in the club (accepted
-- Scoreboard.SubmitXpScore reports, added by fi_db::scores::add_xp_in in the report's transaction):
-- the ladder's score, and the club's XP is its members' sum.
CREATE TABLE game_club_member (
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  club_id    integer NOT NULL,
  role       smallint NOT NULL CHECK (role BETWEEN 1 AND 3),
  joined_at  timestamptz NOT NULL DEFAULT clock_timestamp(),
  xp         double precision NOT NULL DEFAULT 0 CHECK (xp >= 0 AND xp <= 1e15),
  PRIMARY KEY (game, profile_id),
  FOREIGN KEY (game, club_id) REFERENCES game_club (game, id) ON DELETE CASCADE
);

CREATE INDEX game_club_member_club ON game_club_member (game, club_id, role, joined_at);
CREATE UNIQUE INDEX game_club_member_leader ON game_club_member (game, club_id) WHERE role = 1;

-- 4. Invites: `id` is the wire's invitationId (an i64). One pending invite per club and player.
CREATE TABLE game_club_invite (
  game       text NOT NULL REFERENCES game (code),
  id         bigint GENERATED ALWAYS AS IDENTITY NOT NULL,
  club_id    integer NOT NULL,
  profile_id uuid NOT NULL REFERENCES profile (id),
  sender_id  uuid NOT NULL REFERENCES profile (id),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, id),
  UNIQUE (game, club_id, profile_id),
  FOREIGN KEY (game, club_id) REFERENCES game_club (game, id) ON DELETE CASCADE
);

CREATE INDEX game_club_invite_profile ON game_club_invite (game, profile_id);
CREATE INDEX game_club_invite_sender ON game_club_invite (game, sender_id);

-- 5. Requests to join: one per club and player, the first one kept.
CREATE TABLE game_club_request (
  game       text NOT NULL REFERENCES game (code),
  club_id    integer NOT NULL,
  profile_id uuid NOT NULL REFERENCES profile (id),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, club_id, profile_id),
  FOREIGN KEY (game, club_id) REFERENCES game_club (game, id) ON DELETE CASCADE
);

CREATE INDEX game_club_request_profile ON game_club_request (game, profile_id);

-- 6. The notice board: `id` is the wire's notificationId (an i64). Plain text, cleaned, at most 200
-- characters; a club keeps its newest 50. `source_id` is the request's fact id: a replayed
-- CreateNoticeBoardNotification answers the same id.
CREATE TABLE game_club_notice (
  game       text NOT NULL REFERENCES game (code),
  id         bigint GENERATED ALWAYS AS IDENTITY NOT NULL,
  club_id    integer NOT NULL,
  profile_id uuid NOT NULL REFERENCES profile (id),
  message    text NOT NULL CHECK (char_length(message) BETWEEN 1 AND 200),
  source_id  uuid NOT NULL,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, id),
  FOREIGN KEY (game, club_id) REFERENCES game_club (game, id) ON DELETE CASCADE
);

CREATE INDEX game_club_notice_club ON game_club_notice (game, club_id, id);
CREATE INDEX game_club_notice_author ON game_club_notice (game, profile_id);
CREATE UNIQUE INDEX game_club_notice_source ON game_club_notice (game, source_id);

-- 7. A profile's club history (GetClubHistory): the club's name as it was, the role and the action
-- (0 joined, 1 left, 2 removed, 3 role changed, 4 created; the Beta's numbering, the game only
-- shows it). Of the profile's mode at the time; a profile keeps its newest 100 entries. It outlives
-- the club.
CREATE TABLE game_club_history (
  game       text NOT NULL REFERENCES game (code),
  id         bigint GENERATED ALWAYS AS IDENTITY NOT NULL,
  mode       text NOT NULL CHECK (mode IN ('hardcore', 'noob')),
  profile_id uuid NOT NULL REFERENCES profile (id),
  club_name  text NOT NULL CHECK (char_length(club_name) BETWEEN 1 AND 24),
  role       smallint NOT NULL CHECK (role BETWEEN 1 AND 3),
  action     smallint NOT NULL CHECK (action BETWEEN 0 AND 4),
  at         timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, id)
);

CREATE INDEX game_club_history_profile ON game_club_history (game, profile_id, id);

-- 8. Whether a profile takes club invites (User.AcceptClubInvites; no row: it does).
CREATE TABLE game_club_setting (
  game            text NOT NULL REFERENCES game (code),
  profile_id      uuid NOT NULL REFERENCES profile (id),
  accepts_invites boolean NOT NULL,
  PRIMARY KEY (game, profile_id)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON game_club, game_club_member, game_club_invite,
  game_club_request, game_club_notice, game_club_history, game_club_setting TO fi_runtime;
-- The account erasure (fi_maintenance) leaves the erased profile's club as a member's leave does
-- (succession, the empty club deleted) and deletes its rows.
GRANT SELECT, INSERT, UPDATE, DELETE ON game_club, game_club_member, game_club_invite,
  game_club_request, game_club_notice, game_club_history, game_club_setting TO fi_maintenance;
