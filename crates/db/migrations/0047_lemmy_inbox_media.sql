-- Inbox entities need their own Lemmy type namespaces.  Notification ids and
-- direct-status ids must not be accepted where a post/comment id is expected.
ALTER TABLE lemmy_id_aliases DROP CONSTRAINT lemmy_id_aliases_kind_check;
ALTER TABLE lemmy_id_aliases
    ADD CONSTRAINT lemmy_id_aliases_kind_check CHECK (kind BETWEEN 1 AND 10);
COMMENT ON COLUMN lemmy_id_aliases.kind IS
    '1 account, 2 user, 3 status, 4 report, 5 registration, 6 custom emoji, 7 admin action, 8 media, 9 notification, 10 private message';

-- Plamenu's native marker is a high-water mark. Lemmy also lets a client mark
-- one mention/reply read or unread out of order, so retain a sparse override.
CREATE TABLE lemmy_notification_read_overrides (
    account_id      BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    notification_id BIGINT NOT NULL REFERENCES notifications(id) ON DELETE CASCADE,
    read            BOOLEAN NOT NULL,
    PRIMARY KEY (account_id, notification_id)
);

CREATE INDEX lemmy_notification_read_overrides_unread_idx
    ON lemmy_notification_read_overrides (account_id, notification_id DESC)
    WHERE NOT read;

-- Pictrs-compatible deletion is capability based. The token is returned by
-- Lemmy's media-list endpoint as well as at upload time, so it must remain
-- recoverable; database access is already equivalent to access to media.
CREATE TABLE lemmy_media_uploads (
    media_id          BIGINT PRIMARY KEY REFERENCES media_attachments(id) ON DELETE CASCADE,
    account_id        BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    alias              TEXT NOT NULL UNIQUE,
    delete_token       TEXT NOT NULL UNIQUE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX lemmy_media_uploads_account_created_idx
    ON lemmy_media_uploads (account_id, created_at DESC, media_id DESC);

-- Lemmy carries accessibility text and search keywords that Mastodon's emoji
-- row does not. Keep that lossless metadata beside the native emoji.
CREATE TABLE lemmy_custom_emoji_metadata (
    emoji_id   BIGINT PRIMARY KEY REFERENCES custom_emojis(id) ON DELETE CASCADE,
    alt_text   TEXT NOT NULL DEFAULT '',
    keywords   TEXT[] NOT NULL DEFAULT '{}'
);

-- Preferences with no native Plamenu analogue still need to round-trip to a
-- Lemmy client. Native profile/e-mail fields remain authoritative; this JSON
-- stores only compatibility UI preferences such as listing/sort/display mode.
CREATE TABLE lemmy_user_preferences (
    user_id     BIGINT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    preferences JSONB NOT NULL DEFAULT '{}',
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (jsonb_typeof(preferences) = 'object')
);
