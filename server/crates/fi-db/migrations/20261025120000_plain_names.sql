-- OD-120 / CR-S44: plain gamertags (ACCOUNTS.md §3, §5 "Plain names and renames").
--
-- A gamertag is the plain `name`, unique per (game, lower(name)) among profiles that are not
-- erased (an erased tombstone has no name). The 5-digit `discriminator` is no longer written; it
-- stays, with `legacy_name` (the name it went with), only so legacy `Name#12345` input still
-- resolves during the transition. The backfill below gives every clashing profile a free name
-- and a rename notice (one free rename, POST /launcher/v1/profiles/{xuid}/name).

-- New profiles have a name and no discriminator.
ALTER TABLE profile DROP CONSTRAINT profile_gamertag_parts;

ALTER TABLE profile
  -- The name of the legacy `Name#12345` (never cleared until erasure).
  ADD COLUMN legacy_name        text,
  -- Set by the clash backfill while the free rename is unused: the name before it.
  ADD COLUMN rename_notice_from text CHECK (rename_notice_from ~ '^[A-Za-z][A-Za-z0-9]{0,19}$'),
  ADD COLUMN rename_notice_at   timestamptz,
  ADD CONSTRAINT profile_rename_notice_parts CHECK ((rename_notice_from IS NULL) = (rename_notice_at IS NULL)),
  ADD CONSTRAINT profile_legacy_parts CHECK (legacy_name IS NULL OR discriminator IS NOT NULL);

-- Erasure (ACCOUNTS.md §8) also forgets the legacy gamertag and the notice, whoever writes it.
CREATE FUNCTION fi_profile_erase_names() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  IF NEW.status = 'deleted' THEN
    NEW.legacy_name := NULL;
    NEW.discriminator := NULL;
    NEW.rename_notice_from := NULL;
    NEW.rename_notice_at := NULL;
  END IF;
  RETURN NEW;
END $$;
CREATE TRIGGER profile_erase_names BEFORE UPDATE OF status ON profile
  FOR EACH ROW EXECUTE FUNCTION fi_profile_erase_names();

-- The same-device tracking value (CR-S44): HMAC-SHA256 with the pepper of what the launcher sent,
-- never returned.
ALTER TABLE device_binding
  ADD COLUMN fingerprint_hmac bytea CHECK (fingerprint_hmac IS NULL OR length(fingerprint_hmac) = 32);

-- The backfill, in two phases (ACCOUNTS.md §5 item 1). Groups are the profiles of a game whose
-- names are equal case-insensitively. Keep: a profile alone in its group, or the oldest active one
-- (`created_at`, then the lowest XUID), or the oldest pending deletion when none is active.
-- Rename: every other profile, oldest first, takes its own name followed by the lowest number from
-- 2 that no profile of the game holds, the name cut so the number stays within the 15 characters
-- the game shows. Returns the number of profiles renamed. A function so the pg suite can run it on
-- seeded clashes (crates/fi-db/tests/pg_plain_names.rs); it is safe to run again (nothing clashes).
CREATE FUNCTION fi_backfill_plain_names() RETURNS integer LANGUAGE plpgsql AS $$
DECLARE
  r         record;
  n         integer;
  candidate text;
  renamed   integer := 0;
BEGIN
  UPDATE profile SET legacy_name = name
   WHERE name IS NOT NULL AND discriminator IS NOT NULL AND legacy_name IS NULL;
  FOR r IN
    SELECT id, game, name FROM (
      SELECT id, game, name, created_at, xuid,
             row_number() OVER (PARTITION BY game, lower(name)
                                ORDER BY (status = 'active') DESC, created_at, xuid) AS rank
        FROM profile
       WHERE name IS NOT NULL
    ) grouped
     WHERE rank > 1
     ORDER BY game, created_at, xuid
  LOOP
    n := 2;
    LOOP
      candidate := left(r.name, 15 - length(n::text)) || n::text;
      EXIT WHEN NOT EXISTS (
        SELECT 1 FROM profile p
         WHERE p.game = r.game AND lower(p.name) = lower(candidate) AND p.id <> r.id);
      n := n + 1;
    END LOOP;
    UPDATE profile
       SET name = candidate, rename_notice_from = r.name, rename_notice_at = clock_timestamp()
     WHERE id = r.id;
    renamed := renamed + 1;
  END LOOP;
  RETURN renamed;
END $$;

REVOKE EXECUTE ON FUNCTION fi_backfill_plain_names() FROM PUBLIC;
SELECT fi_backfill_plain_names();

-- Only now, with every clash renamed.
DROP INDEX profile_gamertag_key;
CREATE UNIQUE INDEX profile_name_key ON profile (game, lower(name)) WHERE name IS NOT NULL;
-- Legacy `Name#12345` input: the name that went with the number is `legacy_name` once the backfill
-- ran, else `name` (fixtures and imports write a discriminator directly).
CREATE INDEX profile_legacy_gamertag ON profile (game, lower(coalesce(legacy_name, name)), discriminator)
  WHERE discriminator IS NOT NULL;
