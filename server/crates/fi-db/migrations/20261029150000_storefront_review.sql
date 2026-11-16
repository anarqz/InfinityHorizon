-- Storefront review fixes (OD-138 follow-up, 2026-10-29; GAME_PLATFORM.md §3a "Storefront", register
-- CR-S74). Marks are now one per account (`storefront_mark.viewer` = a pseudonymous SHA-256 of the
-- account id), and the gamertag search is a prefix of the lowered name.

-- Erasure deletes an erased account's marks by `viewer`, without a full scan.
CREATE INDEX storefront_mark_viewer ON storefront_mark (viewer);

-- `SearchLiveryFiles` / `SearchLayerGroupFiles` / `SearchTuningFiles` `GamerTag`: `lower(p.name) LIKE
-- 'prefix%'` (text_pattern_ops supports the prefix under any collation).
CREATE INDEX profile_name_prefix ON profile (game, lower(name) text_pattern_ops) WHERE status = 'active';

-- Marks made before this migration were keyed by profile (`sha256('storefront:' || profile id)`):
-- re-keyed to their account (`sha256('storefront-account:' || account id)`, fi_db::storefront::
-- account_key), one per account, file and kind; a mark no profile matches any more is dropped.
INSERT INTO storefront_mark (file_id, kind, viewer, created_at)
SELECT m.file_id, m.kind, sha256(convert_to('storefront-account:' || p.account_id::text, 'UTF8')), min(m.created_at)
  FROM storefront_mark m
  JOIN profile p ON m.viewer = sha256(convert_to('storefront:' || p.id::text, 'UTF8'))
 GROUP BY m.file_id, m.kind, p.account_id
ON CONFLICT DO NOTHING;
DELETE FROM storefront_mark m
 WHERE NOT EXISTS (SELECT 1 FROM account a
                    WHERE m.viewer = sha256(convert_to('storefront-account:' || a.id::text, 'UTF8')));
