-- S26 (OD-181, MATCHMAKING_API.md §2.7, register CR-S84): mixed mod sets in one world. OD-112's
-- equal-set join rule is retired; every player still declares the set its game loads, and the
-- lobby shows each member's set.
--
-- `mm_grant.mods`: the set the seat's join (or the accepted request) declared, validated and sorted
-- as `world.mods` (`[{id, version, fingerprint}]`, at most 32; `[]` = none), fixed for the seat's
-- life. `world_invite.mods`: a request's asker's set at sending (its acceptance seats it with that
-- set; `[]` for invites and vanilla askers). `mm_grant.mods_key` stays the key of the seat's set.
ALTER TABLE mm_grant ADD COLUMN mods jsonb NOT NULL DEFAULT '[]';
ALTER TABLE mm_grant ADD CONSTRAINT mm_grant_mods_shape
  CHECK (jsonb_typeof(mods) = 'array' AND jsonb_array_length(mods) <= 32) NOT VALID;
ALTER TABLE mm_grant VALIDATE CONSTRAINT mm_grant_mods_shape;

ALTER TABLE world_invite ADD COLUMN mods jsonb NOT NULL DEFAULT '[]';
ALTER TABLE world_invite ADD CONSTRAINT world_invite_mods_shape
  CHECK (jsonb_typeof(mods) = 'array' AND jsonb_array_length(mods) <= 32) NOT VALID;
ALTER TABLE world_invite VALIDATE CONSTRAINT world_invite_mods_shape;

-- Under OD-112 a modded seat's key always equalled its world's, so its declared set was the world's:
-- the live seats and pending requests of modded worlds take it (closed worlds' history is left as is).
UPDATE mm_grant g SET mods = w.mods FROM world w
  WHERE g.world_id = w.id AND g.mods_key IS NOT NULL AND g.mods_key = w.mods_key
    AND g.state IN ('issued', 'used') AND w.closed_at IS NULL;
UPDATE world_invite i SET mods = w.mods FROM world w
  WHERE i.world_id = w.id AND i.kind = 'request' AND i.state = 'pending'
    AND w.mods_key IS NOT NULL AND w.closed_at IS NULL;
