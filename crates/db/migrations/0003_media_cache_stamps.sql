-- M39 retention grades: cached remote avatars/headers and emoji images gain
-- a cached-at stamp so the per-class retention periods can evict them
-- (preview_cards.image_cached_at already existed). Existing cached copies
-- are stamped now — they become evictable one full period from this
-- migration, never retroactively. Local images (no remote URL) carry no
-- stamp and are never eviction candidates.

ALTER TABLE accounts
    ADD COLUMN avatar_cached_at timestamp with time zone,
    ADD COLUMN header_cached_at timestamp with time zone;

UPDATE accounts SET avatar_cached_at = now()
    WHERE avatar_file_name IS NOT NULL AND avatar_remote_url IS NOT NULL;
UPDATE accounts SET header_cached_at = now()
    WHERE header_file_name IS NOT NULL AND header_remote_url IS NOT NULL;

ALTER TABLE custom_emojis
    ADD COLUMN image_cached_at timestamp with time zone;

UPDATE custom_emojis SET image_cached_at = now()
    WHERE image_file_name IS NOT NULL AND domain IS NOT NULL;
