-- Exact human-facing URL resolution complements canonical ActivityPub `uri`
-- lookup. Partial indexes keep null-heavy local rows out of these user-driven
-- discovery paths.
CREATE INDEX idx_accounts_url ON accounts (url) WHERE url IS NOT NULL;
CREATE INDEX idx_statuses_url ON statuses (url) WHERE url IS NOT NULL;
