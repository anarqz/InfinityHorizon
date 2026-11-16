-- S33 (MATCHMAKING_API.md §2.5/§3.2, register CR-S86): a seated (`used`) seat that the host's world
-- heartbeat stops listing in `peers` is released only once it has been missing for 30 s in a row
-- (`SEAT_MISSING_GRACE_SECS`), not on the first heartbeat without it. A player moving between the
-- engines of one world (car meet -> free roam) drops out of the host's peers for a moment; live
-- 2026-11-02 16:02 that single miss released the seat and the joiner never reached free roam.
--
-- `mm_grant.missing_since`: when the host's heartbeats first stopped listing this seated peer; NULL
-- while it is listed (or never seated). Set by the first heartbeat without it, cleared by one with
-- it. Nullable without a default: a metadata-only change.
ALTER TABLE mm_grant ADD COLUMN missing_since timestamptz;
