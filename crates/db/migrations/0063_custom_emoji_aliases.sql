-- Owner-scoped historical shortcodes keep already-published profile/post
-- references working after an original personal upload is renamed, without
-- making that retired name available for new composition.
CREATE TABLE custom_emoji_aliases (
    custom_emoji_id bigint NOT NULL REFERENCES custom_emojis(id) ON DELETE CASCADE,
    owner_account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    shortcode text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (custom_emoji_id, owner_account_id, shortcode)
);
CREATE INDEX custom_emoji_aliases_resolve_idx
    ON custom_emoji_aliases (owner_account_id, shortcode);
