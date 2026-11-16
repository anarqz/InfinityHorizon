-- OD-55 (2026-10-19): friendships belong to the account, not the profile. A friendship and a
-- friend request are between two accounts, of every game, and survive every profile change
-- (Hardcore -> Noob, a new profile, a deleted one). Replaces the profile-level tables of
-- 20261019140000_friends.sql, which never held real data (the live stack was not migrated past
-- 20261019130200). Written by core (CORE_API.md §4.1: request by any of the other player's
-- gamertags, accept, decline, remove, favorite; Supporter accounts only, OD-25); read by the game
-- plane through fi_db::friends, where a profile's friends are the profiles of its friend accounts
-- in the same game and mode, visible (GAME_PLATFORM.md §3a "Supporter social").

DROP TABLE friend_request;
DROP TABLE friendship;

-- Two rows per friendship, one per side, so each side keeps its own `favorite`.
CREATE TABLE account_friendship (
  account_id uuid NOT NULL REFERENCES account (id),
  friend_id  uuid NOT NULL REFERENCES account (id),
  favorite   boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (account_id, friend_id),
  CONSTRAINT account_friendship_not_self CHECK (account_id <> friend_id)
);

CREATE INDEX account_friendship_friend ON account_friendship (friend_id);

-- One row from the asker to the asked; it lapses 30 days after `created_at` (fi_db::friends).
CREATE TABLE account_friend_request (
  from_account uuid NOT NULL REFERENCES account (id),
  to_account   uuid NOT NULL REFERENCES account (id),
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (from_account, to_account),
  CONSTRAINT account_friend_request_not_self CHECK (from_account <> to_account)
);

CREATE INDEX account_friend_request_to ON account_friend_request (to_account, created_at);

GRANT SELECT, INSERT, DELETE ON account_friendship, account_friend_request TO fi_runtime;
GRANT UPDATE (favorite) ON account_friendship TO fi_runtime;
GRANT SELECT, DELETE ON account_friendship, account_friend_request TO fi_maintenance;
