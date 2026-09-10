-- Lemmy clients keep per-post read state and can explicitly mark a batch read
-- or unread.  Mastodon's timeline marker cannot represent arbitrary removals,
-- so retain the state as an account/status relation shared by compatibility
-- clients.
CREATE TABLE post_reads (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    status_id  BIGINT NOT NULL REFERENCES statuses(id) ON DELETE CASCADE,
    read_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, status_id)
);

CREATE INDEX post_reads_status_idx ON post_reads (status_id);
