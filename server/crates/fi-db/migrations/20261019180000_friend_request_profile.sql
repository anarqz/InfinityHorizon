-- A friend request names the profile it asked (review fix, CORE_API.md §4.1): the asker's
-- outgoing row shows that profile only, addressed by its XUID, never the other account's handle or
-- its other profiles. Requests are ephemeral (they lapse after 30 days) and were never deployed
-- with data, so the pending ones of test databases are dropped rather than guessed.

DELETE FROM account_friend_request;

ALTER TABLE account_friend_request
  ADD COLUMN asked_profile uuid NOT NULL REFERENCES profile (id);
