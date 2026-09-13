-- An administrator's explicit bulk-rejection choice is materialized before
-- confirmation. This keeps a broad filter from changing underneath the
-- destructive action, while allowing execution to proceed in bounded chunks.
CREATE TABLE admin_account_bulk_selections (
    token text NOT NULL,
    moderator_account_id bigint NOT NULL
        REFERENCES accounts(id) ON DELETE CASCADE,
    account_id bigint NOT NULL,
    outcome text
        CHECK (outcome IS NULL OR outcome IN ('rejected', 'skipped', 'failed')),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (token, account_id)
);

CREATE INDEX admin_account_bulk_selections_created_at_idx
    ON admin_account_bulk_selections (created_at);

CREATE INDEX admin_account_bulk_selections_pending_idx
    ON admin_account_bulk_selections (token, moderator_account_id, account_id)
    WHERE outcome IS NULL;
