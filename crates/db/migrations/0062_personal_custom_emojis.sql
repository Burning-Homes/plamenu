-- Personal custom emoji have an owner, but keep a stable origin identity as
-- they are borrowed, renamed, or promoted.  The origin row deliberately
-- outlives individual copies so trend data can remain deduplicated.
CREATE TABLE custom_emoji_origins (
    id bigint PRIMARY KEY,
    source text NOT NULL CHECK (source IN ('local', 'federated')),
    canonical_uri text,
    created_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO custom_emoji_origins (id, source, canonical_uri, created_at)
SELECT id,
       CASE WHEN domain IS NULL THEN 'local' ELSE 'federated' END,
       uri,
       created_at
FROM custom_emojis;

ALTER TABLE custom_emojis
    ADD COLUMN origin_id bigint,
    ADD COLUMN owner_account_id bigint,
    ADD COLUMN borrowed boolean NOT NULL DEFAULT false,
    ADD COLUMN retired boolean NOT NULL DEFAULT false;

UPDATE custom_emojis SET origin_id = id;

ALTER TABLE custom_emojis
    ALTER COLUMN origin_id SET NOT NULL,
    ADD CONSTRAINT custom_emojis_origin_id_fkey
        FOREIGN KEY (origin_id) REFERENCES custom_emoji_origins(id),
    ADD CONSTRAINT custom_emojis_owner_account_id_fkey
        FOREIGN KEY (owner_account_id) REFERENCES accounts(id) ON DELETE CASCADE,
    ADD CONSTRAINT custom_emojis_personal_is_local
        CHECK (owner_account_id IS NULL OR domain IS NULL);

ALTER TABLE custom_emojis
    DROP CONSTRAINT custom_emojis_shortcode_domain_key;

CREATE UNIQUE INDEX custom_emojis_remote_shortcode_domain
    ON custom_emojis (shortcode, domain)
    WHERE domain IS NOT NULL;
CREATE UNIQUE INDEX custom_emojis_instance_shortcode
    ON custom_emojis (shortcode)
    WHERE domain IS NULL AND owner_account_id IS NULL;
CREATE UNIQUE INDEX custom_emojis_personal_owner_shortcode
    ON custom_emojis (owner_account_id, shortcode)
    WHERE owner_account_id IS NOT NULL AND NOT retired;
CREATE UNIQUE INDEX custom_emojis_personal_owner_origin
    ON custom_emojis (owner_account_id, origin_id)
    WHERE owner_account_id IS NOT NULL AND NOT retired;
CREATE INDEX custom_emojis_origin_idx ON custom_emojis (origin_id);
CREATE INDEX custom_emojis_owner_idx
    ON custom_emojis (owner_account_id, shortcode)
    WHERE owner_account_id IS NOT NULL;

-- Kept separate from the already-wide instance_settings row.  Zero means
-- unlimited.  There is exactly one settings row for the installation.
CREATE TABLE custom_emoji_settings (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    personal_limit integer NOT NULL DEFAULT 50 CHECK (personal_limit >= 0)
);
INSERT INTO custom_emoji_settings (singleton, personal_limit) VALUES (true, 50);

-- Exact, bounded seven-day usage accounting.  One row per origin/user/day
-- prevents the trend query from scanning statuses or reactions.
CREATE TABLE custom_emoji_usages (
    origin_id bigint NOT NULL REFERENCES custom_emoji_origins(id) ON DELETE CASCADE,
    day date NOT NULL DEFAULT CURRENT_DATE,
    account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    post_uses integer NOT NULL DEFAULT 0 CHECK (post_uses >= 0),
    reaction_uses integer NOT NULL DEFAULT 0 CHECK (reaction_uses >= 0),
    PRIMARY KEY (origin_id, day, account_id)
);
CREATE INDEX custom_emoji_usages_recent_idx ON custom_emoji_usages (day, origin_id);

-- Custom reactions are grouped by lineage, not merely shortcode. Two users
-- can therefore react with different personal images that share a name.
ALTER TABLE status_reactions
    ADD COLUMN custom_emoji_id bigint REFERENCES custom_emojis(id) ON DELETE SET NULL,
    ADD COLUMN custom_emoji_origin_id bigint REFERENCES custom_emoji_origins(id) ON DELETE SET NULL;
ALTER TABLE status_reactions
    DROP CONSTRAINT status_reactions_account_id_status_id_name_key;
CREATE UNIQUE INDEX status_reactions_unicode_identity
    ON status_reactions (account_id, status_id, name)
    WHERE custom_emoji_origin_id IS NULL;
CREATE UNIQUE INDEX status_reactions_custom_identity
    ON status_reactions (account_id, status_id, custom_emoji_origin_id)
    WHERE custom_emoji_origin_id IS NOT NULL;

-- The built-in User role receives personal upload/borrow by default.  This is
-- a Plamenu extension bit, not a Mastodon-compatible staff permission.
UPDATE user_roles SET permissions = permissions | (1::bigint << 21) WHERE id = 4;
