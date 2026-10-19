-- Post excerpts remain in the bounded catalogue, separate from delivered statuses.
-- Outbox routing evidence belongs to the already retained canonical actor.
ALTER TABLE discovery_entries
    ADD COLUMN outbox_uri text CHECK (outbox_uri IS NULL OR (kind='profile' AND octet_length(outbox_uri) BETWEEN 1 AND 2048)),
    ADD COLUMN outbox_next_at timestamptz,
    ADD COLUMN published_at timestamptz,
    ADD COLUMN sensitive boolean NOT NULL DEFAULT false;
CREATE INDEX discovery_entries_author ON discovery_entries(actor_uri) WHERE kind='post';
CREATE INDEX discovery_entries_search ON discovery_entries USING gin
    (to_tsvector('simple', coalesce(title,'') || ' ' || coalesce(summary,'')));
-- Erasure includes the index; minimal identity/refusal records still consume quota.
UPDATE discovery_settings SET revision=revision+1;
UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL;
