-- CR-S77 (OD-137, owner 2026-10-29: "Prévia do Storefront"; ACCOUNTS.md §5a): a random 128-bit token per
-- Storefront file names its public preview image (`GET /media/v1/previews/{token}.png`). Never derived from an id:
-- 16 bytes of SHA-256 over two random UUIDs (gen_random_uuid draws from the server's strong random source), so every
-- existing row gets its own value now and every new row one at insert.
ALTER TABLE storefront_file
  ADD COLUMN preview_token bytea NOT NULL
    DEFAULT substring(sha256(uuid_send(gen_random_uuid()) || uuid_send(gen_random_uuid())) FROM 1 FOR 16)
    CONSTRAINT storefront_file_preview_token_shape CHECK (octet_length(preview_token) = 16);

CREATE UNIQUE INDEX storefront_file_preview_token ON storefront_file (preview_token);
