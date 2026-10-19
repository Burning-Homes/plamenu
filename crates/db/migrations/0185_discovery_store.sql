-- Independent discovery stays disabled until an operator enables it. All
-- admission transactions lock this singleton before counting or allocating.
CREATE TABLE discovery_settings (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    enabled boolean NOT NULL DEFAULT false,
    paused boolean NOT NULL DEFAULT false,
    allow_missing_discoverable boolean NOT NULL DEFAULT true,
    allow_missing_indexable boolean NOT NULL DEFAULT true,
    disabled_mechanisms text[] NOT NULL DEFAULT '{}'
        CHECK (disabled_mechanisms <@ ARRAY['source_inspection','peer_exploration',
            'directory','outbox','metadata_cache','profile_listing',
            'public_post_indexing','recommendations','peer_sharing']::text[]
            AND cardinality(disabled_mechanisms) <= 9
            AND array_position(disabled_mechanisms, NULL) IS NULL),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    max_sources bigint NOT NULL DEFAULT 100 CHECK (max_sources BETWEEN 1 AND 100000),
    sources_per_origin bigint NOT NULL DEFAULT 4 CHECK (sources_per_origin BETWEEN 1 AND 100000),
    sources_per_introducer bigint NOT NULL DEFAULT 20 CHECK (sources_per_introducer BETWEEN 1 AND 100000),
    max_jobs bigint NOT NULL DEFAULT 1000 CHECK (max_jobs BETWEEN 1 AND 100000),
    jobs_per_origin bigint NOT NULL DEFAULT 100 CHECK (jobs_per_origin BETWEEN 1 AND 100000),
    jobs_per_introducer bigint NOT NULL DEFAULT 100 CHECK (jobs_per_introducer BETWEEN 1 AND 100000),
    max_entries bigint NOT NULL DEFAULT 200 CHECK (max_entries BETWEEN 1 AND 1000000),
    entries_per_origin bigint NOT NULL DEFAULT 40 CHECK (entries_per_origin BETWEEN 1 AND 1000000),
    entries_per_introducer bigint NOT NULL DEFAULT 100 CHECK (entries_per_introducer BETWEEN 1 AND 1000000),
    max_inflight integer NOT NULL DEFAULT 2 CHECK (max_inflight BETWEEN 1 AND 128),
    max_attempts integer NOT NULL DEFAULT 5 CHECK (max_attempts BETWEEN 1 AND 100),
    lease_seconds integer NOT NULL DEFAULT 120 CHECK (lease_seconds BETWEEN 1 AND 3600)
);
INSERT INTO discovery_settings (singleton) VALUES (true);

CREATE TABLE discovery_sources (
    id bigint PRIMARY KEY,
    origin text NOT NULL CHECK (octet_length(origin) BETWEEN 1 AND 512),
    endpoint text NOT NULL UNIQUE CHECK (octet_length(endpoint) BETWEEN 1 AND 2048),
    introduced_by bigint REFERENCES discovery_sources(id),
    enabled boolean NOT NULL DEFAULT true,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    canonical_uri text CHECK (octet_length(canonical_uri) BETWEEN 1 AND 2048),
    capabilities text[] NOT NULL DEFAULT '{}'
        CHECK (capabilities <@ ARRAY['source_inspection','peer_exploration','directory','outbox']::text[]
            AND cardinality(capabilities) <= 4 AND array_position(capabilities, NULL) IS NULL),
    verified_at timestamptz,
    valid_until timestamptz,
    cooldown_until timestamptz,
    last_error text CHECK (octet_length(last_error) <= 128),
    attempts_total bigint NOT NULL DEFAULT 0 CHECK (attempts_total >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((verified_at IS NULL AND valid_until IS NULL AND canonical_uri IS NULL)
        OR (verified_at IS NOT NULL AND valid_until IS NOT NULL
            AND valid_until > verified_at AND canonical_uri IS NOT NULL))
);
CREATE INDEX discovery_sources_origin ON discovery_sources(origin);
CREATE INDEX discovery_sources_introducer ON discovery_sources(introduced_by);

CREATE TABLE discovery_entries (
    id bigint PRIMARY KEY,
    canonical_uri text NOT NULL UNIQUE CHECK (octet_length(canonical_uri) BETWEEN 1 AND 2048),
    source_id bigint NOT NULL REFERENCES discovery_sources(id),
    kind text NOT NULL CHECK (kind IN ('source','profile','post')),
    actor_uri text CHECK (octet_length(actor_uri) BETWEEN 1 AND 2048),
    account_id bigint REFERENCES accounts(id) ON DELETE SET NULL,
    state text NOT NULL DEFAULT 'active' CHECK (state IN ('active','denied','withdrawn')),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    policy_revision bigint NOT NULL,
    discoverable text NOT NULL CHECK (discoverable IN ('missing','allow','deny','invalid')),
    indexable text NOT NULL CHECK (indexable IN ('missing','allow','deny','invalid')),
    visibility text NOT NULL CHECK (visibility IN ('public','unlisted','private','direct','local','members','unknown')),
    title text CHECK (octet_length(title) <= 1024),
    summary text CHECK (octet_length(summary) <= 8192),
    language text CHECK (octet_length(language) <= 64),
    verified_at timestamptz NOT NULL,
    valid_until timestamptz NOT NULL CHECK (valid_until > verified_at),
    invalidation_reason text CHECK (octet_length(invalidation_reason) <= 128),
    updated_at timestamptz NOT NULL DEFAULT now(),
    -- Only already-verified public payload belongs here. The server evaluates
    -- current per-use policy again before displaying it; this is not a feed.
    CHECK ((state='active' AND actor_uri IS NOT NULL AND title IS NOT NULL AND summary IS NOT NULL)
        OR (state<>'active' AND title IS NULL AND summary IS NULL AND language IS NULL))
);
CREATE INDEX discovery_entries_source ON discovery_entries(source_id);
CREATE INDEX discovery_entries_expiry ON discovery_entries(valid_until) WHERE state='active';

CREATE TABLE discovery_jobs (
    id bigint PRIMARY KEY,
    source_id bigint NOT NULL REFERENCES discovery_sources(id),
    target_uri text NOT NULL CHECK (octet_length(target_uri) BETWEEN 1 AND 2048),
    origin text NOT NULL CHECK (octet_length(origin) BETWEEN 1 AND 512),
    introduced_by bigint REFERENCES discovery_sources(id),
    operation text NOT NULL CHECK (operation IN ('inspect_source','list_peers','list_directory','sample_outbox','refresh_entry')),
    cursor text CHECK (octet_length(cursor) <= 2048),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    run_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL DEFAULT now() + interval '7 days',
    lease_token bigint,
    lease_owner text CHECK (octet_length(lease_owner) BETWEEN 1 AND 128),
    leased_until timestamptz,
    policy_revision bigint,
    source_revision bigint,
    entry_revision bigint,
    last_error text CHECK (octet_length(last_error) <= 128),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (target_uri,operation),
    CHECK ((lease_token IS NULL AND lease_owner IS NULL AND leased_until IS NULL)
        OR (lease_token IS NOT NULL AND lease_owner IS NOT NULL AND leased_until IS NOT NULL
            AND policy_revision IS NOT NULL AND source_revision IS NOT NULL))
);
CREATE INDEX discovery_jobs_due ON discovery_jobs(run_at,id);
CREATE INDEX discovery_jobs_origin ON discovery_jobs(origin);
CREATE INDEX discovery_jobs_introducer ON discovery_jobs(introduced_by);
CREATE INDEX discovery_jobs_source ON discovery_jobs(source_id);
CREATE INDEX discovery_jobs_expiry ON discovery_jobs(expires_at);
