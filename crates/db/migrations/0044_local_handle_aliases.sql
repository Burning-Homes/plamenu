-- Human-profile redirects for renameable immutable-ID local actors. These
-- aliases are discovery metadata only and never become ActivityPub actor IDs.
CREATE TABLE local_handle_aliases (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    username TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, username)
);

CREATE UNIQUE INDEX local_handle_aliases_username_lower_idx
    ON local_handle_aliases (lower(username));
