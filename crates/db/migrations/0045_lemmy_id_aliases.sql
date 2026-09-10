-- Lemmy exposes database identifiers as signed 32-bit JSON numbers, while
-- Plamenu's native/Mastodon identifiers are 64-bit snowflakes.  Keep a durable
-- API-only namespace rather than truncating snowflakes or changing canonical
-- ActivityPub identities.
CREATE TABLE lemmy_id_aliases (
    lemmy_id   INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind       SMALLINT NOT NULL CHECK (kind BETWEEN 1 AND 8),
    plamenu_id BIGINT NOT NULL CHECK (plamenu_id > 0),
    UNIQUE (kind, plamenu_id)
);

COMMENT ON TABLE lemmy_id_aliases IS
    'Stable, never-reused i32 aliases for entities exposed through /api/v3';
COMMENT ON COLUMN lemmy_id_aliases.kind IS
    '1 account, 2 user, 3 status, 4 report, 5 registration, 6 custom emoji, 7 admin action, 8 media';
