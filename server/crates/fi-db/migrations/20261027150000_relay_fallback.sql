-- Relay fallback (2026-10-27; CR-S57; owner 2026-10-26 "an official relay VM in São Paulo,
-- community relays, automatic fallback"; docs/contracts/MATCHMAKING_API.md §5b).
--
-- A world records its host's NAT test (`nat`, NULL = not reported) and its transport, fixed when
-- it opens: `relay` (a symmetric host: the lobby is born on a relay) names the world's relay
-- (`relay_key`, the relay's public key; `relay_name`, the operator's name for it, display only).
-- `relays` holds the host's latest measured round trips to the relays the server trusts
-- ({publicKey: ms}; NULL = never reported), read when a joiner's relay is chosen.
--
-- A seat records the relay its pair meets at (`relay_key`, NULL = none) and its transport now
-- (`relay` once the joiner fell back, or when born on a relay). Existing rows are direct.
ALTER TABLE world ADD COLUMN nat text;
ALTER TABLE world ADD COLUMN transport text NOT NULL DEFAULT 'direct';
ALTER TABLE world ADD COLUMN relay_key text;
ALTER TABLE world ADD COLUMN relay_name text;
ALTER TABLE world ADD COLUMN relays jsonb;
ALTER TABLE world ADD CONSTRAINT world_relay_shape CHECK (
  (nat IS NULL OR nat IN ('open', 'restricted', 'symmetric'))
  AND transport IN ('direct', 'relay')
  AND ((transport = 'relay') = (relay_key IS NOT NULL))
  AND (relay_key IS NULL OR relay_key ~ '^[A-Za-z0-9+/]{43}=$')
  AND (relay_name IS NULL OR char_length(relay_name) BETWEEN 1 AND 48)
  AND (relays IS NULL OR jsonb_typeof(relays) = 'object')
);

ALTER TABLE mm_grant ADD COLUMN relay_key text;
ALTER TABLE mm_grant ADD COLUMN transport text NOT NULL DEFAULT 'direct';
ALTER TABLE mm_grant ADD CONSTRAINT mm_grant_relay_shape CHECK (
  transport IN ('direct', 'relay')
  AND (transport = 'direct' OR relay_key IS NOT NULL)
  AND (relay_key IS NULL OR relay_key ~ '^[A-Za-z0-9+/]{43}=$')
);
