-- The Storefront: players share designs, vinyl groups and tunes again (owner decision OD-138,
-- 2026-10-29: "BETA worked properly. It allowed players to share their liveries using custom
-- keywords collected from the server"; "Everything should be shared, vinyl groups and tunes";
-- GAME_PLATFORM.md §3a "Storefront", register CR-S74). Ported from the Beta's db/storefront.rs.
-- Written only through fi_db::storefront (and fi_db::liveries for the livery exemptions).
--
-- 1. The libraries the game syncs: vinyl groups (OnlineProfile.UpdateLiveryGroup, by name) and car
--    setups (UpdateCarSetup, by car and name). CreateVersionedLayerGroup / CreateVersionedTune
--    send only a name, so the server keeps what they name, as UpdateLivery's drafts do.
CREATE TABLE game_layer_group_draft (
  game             text NOT NULL REFERENCES game (code),
  profile_id       uuid NOT NULL REFERENCES profile (id),
  name             text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 64),
  number_of_layers smallint NOT NULL CHECK (number_of_layers >= 0),
  -- VersionedLayerGroupData.data's max_len in the game's schema: 256 KiB.
  data             bytea NOT NULL CHECK (octet_length(data) BETWEEN 1 AND 262144),
  updated_at       timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (profile_id, name)
);

CREATE TABLE game_car_setup (
  game           text NOT NULL REFERENCES game (code),
  profile_id     uuid NOT NULL REFERENCES profile (id),
  car_id         integer NOT NULL,
  name           text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 64),
  binary_version smallint NOT NULL,
  -- The game's CInstalledParts and CCarTuneDef as decoded (re-encoded on retrieve).
  installed_parts jsonb NOT NULL,
  tuning          jsonb NOT NULL,
  updated_at     timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (profile_id, car_id, name)
);

-- 2. Versioned content: what a shared file points at and other games fetch by id. Ids are 62 bits
--    of the request's fact id (a replayed request answers the same id), as game_livery's.
CREATE TABLE game_layer_group (
  id               bigint PRIMARY KEY CHECK (id > 0),
  game             text NOT NULL REFERENCES game (code),
  profile_id       uuid NOT NULL REFERENCES profile (id),
  name             text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 64),
  number_of_layers smallint NOT NULL CHECK (number_of_layers >= 0),
  data             bytea NOT NULL CHECK (octet_length(data) BETWEEN 1 AND 262144),
  thumbnail        bytea CHECK (thumbnail IS NULL OR octet_length(thumbnail) BETWEEN 1 AND 409600),
  hidden_at        timestamptz,
  created_at       timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX game_layer_group_profile ON game_layer_group (profile_id, created_at DESC);

CREATE TABLE game_tune (
  id              bigint PRIMARY KEY CHECK (id > 0),
  game            text NOT NULL REFERENCES game (code),
  profile_id      uuid NOT NULL REFERENCES profile (id),
  car_id          integer NOT NULL,
  name            text NOT NULL CHECK (char_length(name) <= 64),
  binary_version  smallint NOT NULL,
  installed_parts jsonb NOT NULL,
  tuning          jsonb NOT NULL,
  hidden_at       timestamptz,
  created_at      timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX game_tune_profile ON game_tune (profile_id, created_at DESC);

-- 3. Shared files (StorefrontLivery / StorefrontLayerGroup / StorefrontTuning.Create): one per
--    shared item, pointing at exactly one of a livery, a vinyl group or a tune, and gone with it.
--    A shared item is never pruned while its file lives (the Beta's rule: "content is kept while
--    shared"). `removed_at`: taken off by reports, under the moderators' review.
CREATE TABLE storefront_file (
  id           integer GENERATED ALWAYS AS IDENTITY (MAXVALUE 2147483647) PRIMARY KEY,
  game         text NOT NULL REFERENCES game (code),
  profile_id   uuid NOT NULL REFERENCES profile (id),
  kind         text NOT NULL CHECK (kind IN ('livery', 'group', 'tune')),
  livery_id    bigint UNIQUE REFERENCES game_livery (id) ON DELETE CASCADE,
  group_id     bigint UNIQUE REFERENCES game_layer_group (id) ON DELETE CASCADE,
  tune_id      bigint UNIQUE REFERENCES game_tune (id) ON DELETE CASCADE,
  -- ForzaStorefrontFileHeader.Type as the game sends it (2 = a design).
  file_type    integer NOT NULL,
  car_id       integer NOT NULL,
  title        text NOT NULL CHECK (char_length(title) <= 16),
  description  text NOT NULL CHECK (char_length(description) <= 256),
  keyword_one  smallint NOT NULL,
  keyword_two  smallint NOT NULL,
  layers       smallint NOT NULL DEFAULT 0,
  -- A tune's search fields (MakeId, ModelId, ClassId, EnvironmentId, TrackId, PerformanceIndex).
  meta         jsonb NOT NULL DEFAULT '{}',
  preview      bytea CHECK (preview IS NULL OR octet_length(preview) BETWEEN 1 AND 409600),
  downloads    integer NOT NULL DEFAULT 0 CHECK (downloads >= 0),
  likes        integer NOT NULL DEFAULT 0 CHECK (likes >= 0),
  uses         integer NOT NULL DEFAULT 0 CHECK (uses >= 0),
  views        integer NOT NULL DEFAULT 0 CHECK (views >= 0),
  strikes      smallint NOT NULL DEFAULT 0 CHECK (strikes >= 0),
  removed_at   timestamptz,
  created_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
  CONSTRAINT storefront_file_one_item CHECK (
    (kind = 'livery' AND livery_id IS NOT NULL AND group_id IS NULL AND tune_id IS NULL) OR
    (kind = 'group' AND group_id IS NOT NULL AND livery_id IS NULL AND tune_id IS NULL) OR
    (kind = 'tune' AND tune_id IS NOT NULL AND livery_id IS NULL AND group_id IS NULL))
);
CREATE INDEX storefront_file_list ON storefront_file (game, kind, created_at DESC) WHERE removed_at IS NULL;
CREATE INDEX storefront_file_profile ON storefront_file (profile_id, created_at DESC);

-- 4. Who counted what, once each: downloads, likes, uses, views and reports. The viewer is the
--    SHA-256 of its profile id (no profile id stays behind an erased viewer).
CREATE TABLE storefront_mark (
  file_id    integer NOT NULL REFERENCES storefront_file (id) ON DELETE CASCADE,
  kind       text NOT NULL CHECK (kind IN ('download', 'like', 'use', 'view', 'report')),
  viewer     bytea NOT NULL CHECK (length(viewer) = 32),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (file_id, kind, viewer)
);

-- 5. Followed creators (AddFavoriteUser, ToggleFollowedCreator): the follower as its hash, the
--    creator's profile; erasure deletes the creator's rows.
CREATE TABLE storefront_follow (
  follower   bytea NOT NULL CHECK (length(follower) = 32),
  creator_id uuid NOT NULL REFERENCES profile (id),
  created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
  PRIMARY KEY (follower, creator_id)
);
CREATE INDEX storefront_follow_creator ON storefront_follow (creator_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON game_layer_group_draft, game_car_setup, game_layer_group,
  game_tune, storefront_file, storefront_mark, storefront_follow TO fi_runtime;
-- The account erasure (fi_maintenance) deletes an erased profile's rows.
GRANT SELECT, DELETE ON game_layer_group_draft, game_car_setup, game_layer_group, game_tune,
  storefront_file, storefront_mark, storefront_follow TO fi_maintenance;
