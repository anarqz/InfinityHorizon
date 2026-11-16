-- FM5 cloud saves (M11, OD-219, register CR-S105; SAVES_API.md §4.12, CLOUD_SAVES.md §3.4): an
-- FM5 Noob career's line stores FM5SAVE1 bundles. `save_blob.format` names each bundle's format
-- (written from the line's game by fi-db's insert_blob; rows before this migration are all
-- fh2save1), and an FM5 bundle may be larger than FH2's 16 MiB ceiling: it carries the garage's
-- livery and thumbnail containers (PNG thumbnails do not pack), at most 48 MiB
-- (`fi_saves::fm5::MAX_BUNDLE`). FH2's ceiling is unchanged.
--
-- Forward only: 20261017000400_saves.sql is applied and immutable (PRODUCTION_MIGRATIONS.json).
-- Both constraints are the column constraints that migration declared, under PostgreSQL's names.

ALTER TABLE save_blob DROP CONSTRAINT save_blob_format_check;
ALTER TABLE save_blob ADD CONSTRAINT save_blob_format_check
  CHECK (format IN ('fh2save1', 'fm5save1'));

ALTER TABLE save_blob DROP CONSTRAINT save_blob_size_bytes_check;
ALTER TABLE save_blob ADD CONSTRAINT save_blob_size_bytes_check
  CHECK (size_bytes >= 1 AND size_bytes <= CASE format WHEN 'fm5save1' THEN 50331648 ELSE 16777216 END);
