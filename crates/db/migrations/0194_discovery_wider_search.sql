-- One short-lived receipt per member; no query text, credentials or history.
CREATE TABLE discovery_search_runs (
    id bigint PRIMARY KEY,
    account_id bigint NOT NULL UNIQUE REFERENCES accounts(id) ON DELETE CASCADE,
    query_digest text NOT NULL CHECK (length(query_digest)=64),
    kind text NOT NULL CHECK (kind IN ('profile','community')),
    source_ids bigint[] NOT NULL CHECK (cardinality(source_ids) BETWEEN 1 AND 3 AND array_position(source_ids,NULL) IS NULL),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    deadline timestamptz NOT NULL DEFAULT clock_timestamp()+interval '15 seconds',
    expires_at timestamptz NOT NULL DEFAULT clock_timestamp()+interval '5 minutes',
    requests integer NOT NULL DEFAULT 0 CHECK (requests BETWEEN 0 AND 24),
    finished boolean NOT NULL DEFAULT false,
    results jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(results)='array' AND jsonb_array_length(results)<=3 AND octet_length(results::text)<=4096)
);
-- Capability failures don't disable the source's working directory adapter.
CREATE TABLE discovery_search_sources (
    source_id bigint NOT NULL REFERENCES discovery_sources(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('profile','community')),
    outcome text NOT NULL CHECK (outcome IN ('ready','unsupported','authentication','rate_limited','unavailable')),
    retry_at timestamptz NOT NULL,
    checked_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY(source_id,kind)
);
ALTER TABLE discovery_jobs ADD COLUMN search_run_id bigint REFERENCES discovery_search_runs(id) ON DELETE CASCADE;
ALTER TABLE discovery_jobs ADD CHECK (NOT withdrawn OR search_run_id IS NULL);
ALTER TABLE discovery_jobs DROP CONSTRAINT discovery_jobs_operation_check;
ALTER TABLE discovery_jobs ADD CHECK (operation IN ('inspect_source','list_peers','list_directory',
    'list_communities','sample_outbox','refresh_entry','revalidate_entry','search_source'));
ALTER TABLE discovery_settings DROP CONSTRAINT discovery_settings_disabled_mechanisms_check;
ALTER TABLE discovery_settings ADD CHECK (
    disabled_mechanisms <@ ARRAY['source_inspection','peer_exploration','directory',
        'community_directory','outbox','metadata_cache','profile_listing',
        'public_post_indexing','recommendations','peer_sharing','wider_search']::text[]
    AND cardinality(disabled_mechanisms)<=11 AND array_position(disabled_mechanisms,NULL) IS NULL);
UPDATE discovery_settings SET revision=revision+1;
UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL;
