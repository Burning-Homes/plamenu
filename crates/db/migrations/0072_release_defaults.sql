-- Move instance settings still at their previous defaults to the release
-- defaults. Different operator values are retained. Existing user preferences
-- are never rewritten: the users column default applies only to new accounts.
ALTER TABLE instance_settings
    ALTER COLUMN max_characters SET DEFAULT 5000,
    ALTER COLUMN media_remote_full_processing SET DEFAULT 'avif',
    ALTER COLUMN media_remote_gif_handling SET DEFAULT 'webp',
    ALTER COLUMN media_avif_quality SET DEFAULT 70;

UPDATE instance_settings SET
    max_characters = CASE WHEN max_characters = 500 THEN 5000 ELSE max_characters END,
    media_remote_full_processing = CASE
        WHEN media_remote_full_processing = 'passthrough' THEN 'avif'
        ELSE media_remote_full_processing END,
    media_remote_gif_handling = CASE
        WHEN media_remote_gif_handling = 'keep' THEN 'webp'
        ELSE media_remote_gif_handling END,
    media_avif_quality = CASE WHEN media_avif_quality = 60 THEN 70 ELSE media_avif_quality END;

ALTER TABLE remote_history_settings
    ALTER COLUMN enabled SET DEFAULT true,
    ALTER COLUMN bare_iri_enabled SET DEFAULT true;
UPDATE remote_history_settings SET enabled = true, bare_iri_enabled = true;

ALTER TABLE custom_emoji_settings ALTER COLUMN max_file_size_kb SET DEFAULT 512;
UPDATE custom_emoji_settings SET max_file_size_kb = 512 WHERE max_file_size_kb = 256;

ALTER TABLE users ALTER COLUMN reading_allow_direct_remote_media SET DEFAULT true;

-- Add MANAGE_GROUPS to the built-in Admin's previous default permission set.
-- Preserve roles whose name or permissions have been customized.
UPDATE user_roles SET permissions = permissions | (1::bigint << 20)
WHERE id = 2 AND name = 'Admin' AND permissions = 13631486;
