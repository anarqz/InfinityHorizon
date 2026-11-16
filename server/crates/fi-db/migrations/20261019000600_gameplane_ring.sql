-- The game plane, part 2: the kept replies of Transaction.Reconcile become a bounded ring (review
-- of the game plane, server/REVIEW_LOG.md "SER-M3 steps 3-4 review" G3; GAME_PLATFORM.md §3a).
--
-- 20261019000400 kept every reply of a request with X-TransactionId, without a per-profile bound,
-- and the runtime role cannot delete. Now only the methods the game reconciles keep their reply
-- (User.RetrieveOnlineCredits, the only one the Beta's captures show with a transaction id), in a
-- ring of 8 slots per profile upserted by slot: a retried transaction id rewrites its own slot, a
-- new one takes a free slot, else the oldest. Nothing in the game plane then grows without bound
-- per profile; a retention job is needed only to drop the rows of erased profiles.
--
-- Written as a new migration, not an edit of 20261019000400, because that one is already applied
-- on the local stacks (sqlx refuses an edited applied migration). The kept replies are a cache of
-- answers the game may ask for again; dropping them only means a Reconcile after this migration
-- answers Xls error 6, as for any unknown transaction.

DELETE FROM game_transaction;

DROP INDEX IF EXISTS game_transaction_age;

ALTER TABLE game_transaction
  ADD COLUMN slot smallint NOT NULL CHECK (slot BETWEEN 0 AND 7),
  DROP CONSTRAINT game_transaction_pkey,
  ADD PRIMARY KEY (game, profile_id, slot),
  ADD CONSTRAINT game_transaction_id UNIQUE (game, profile_id, transaction_id);

COMMENT ON TABLE game_transaction IS
  'Replies kept for Xls Transaction.Reconcile: a ring of 8 slots per profile (20261019000600).';
