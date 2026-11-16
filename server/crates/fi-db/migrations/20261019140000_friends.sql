-- SER-M3 step 7 (OD-48 item 2; CORE_API.md §4, PORTAL_API.md §8 POR-M5): the friend graph of the
-- Supporters' universe. Friends are profiles of one game (any mode: the universe's reads filter
-- by mode with fi_db::visibility). A friendship is two rows, one per side, so each side keeps its
-- own `favorite`; a request is one row from the asker to the asked. Written by core (the
-- launcher's and the web's friend routes); read by the game plane (friend views of the boards,
-- drivatars and rivals friend-first, compare stats, the Xbox Live people and presence, the
-- inbox's rivals sync and messages) through fi_db::friends.

CREATE TABLE friendship (
  game       text NOT NULL REFERENCES game (code),
  profile_id uuid NOT NULL REFERENCES profile (id),
  friend_id  uuid NOT NULL REFERENCES profile (id),
  favorite   boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, profile_id, friend_id),
  CONSTRAINT friendship_not_self CHECK (profile_id <> friend_id)
);

CREATE INDEX friendship_friend ON friendship (game, friend_id);

CREATE TABLE friend_request (
  game         text NOT NULL REFERENCES game (code),
  from_profile uuid NOT NULL REFERENCES profile (id),
  to_profile   uuid NOT NULL REFERENCES profile (id),
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (game, from_profile, to_profile),
  CONSTRAINT friend_request_not_self CHECK (from_profile <> to_profile)
);

CREATE INDEX friend_request_to ON friend_request (game, to_profile, created_at);

GRANT SELECT, INSERT, DELETE ON friendship, friend_request TO fi_runtime;
GRANT UPDATE (favorite) ON friendship TO fi_runtime;
GRANT SELECT, DELETE ON friendship, friend_request TO fi_maintenance;
