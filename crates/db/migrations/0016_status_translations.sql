-- Persistent status-translation cache (TRANSLATION_CACHE_PLAN.md): each
-- (status, target language) pair is translated once per edit ever, across
-- restarts and users. `source_hash` is a sha256 over the exact fragments sent
-- to the backend; a stored row whose hash no longer matches the status'
-- current fragments is treated as a miss, so edits invalidate implicitly.
-- Media descriptions ride as parallel arrays (media_ids[i] ↔
-- media_descriptions[i]) to keep the row fully typed.
CREATE TABLE status_translations (
    status_id BIGINT NOT NULL REFERENCES statuses (id) ON DELETE CASCADE,
    target_language TEXT NOT NULL,
    source_hash BYTEA NOT NULL,
    provider TEXT NOT NULL,
    detected_source_language TEXT,
    content TEXT NOT NULL DEFAULT '',
    spoiler_text TEXT NOT NULL DEFAULT '',
    poll_options TEXT[] NOT NULL DEFAULT '{}',
    media_ids BIGINT[] NOT NULL DEFAULT '{}',
    media_descriptions TEXT[] NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (status_id, target_language)
);

-- Retention pruning and the row-cap eviction both walk this.
CREATE INDEX status_translations_last_used ON status_translations (last_used_at);

-- Operator knobs for the cache and for protecting a slow self-hosted backend.
ALTER TABLE instance_settings
    ADD COLUMN translation_cache_retention_days integer NOT NULL DEFAULT 30,
    ADD COLUMN translation_cache_max_rows integer NOT NULL DEFAULT 200000,
    ADD COLUMN translation_backend_concurrency integer NOT NULL DEFAULT 2,
    ADD COLUMN translation_user_rate_limit_per_hour integer NOT NULL DEFAULT 60,
    ADD COLUMN translation_refresh_on_provider_change boolean NOT NULL DEFAULT false;

-- Per-user "translate posts into" preference; NULL follows the default
-- posting language, which is what the thread translate link targeted before.
ALTER TABLE users
    ADD COLUMN reading_translate_language TEXT;
