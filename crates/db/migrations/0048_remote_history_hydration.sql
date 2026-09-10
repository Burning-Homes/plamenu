-- Bounded, demand-driven hydration of remote ActivityPub outboxes.
--
-- Historical rows are ordinary canonical statuses with explicit provenance.
-- They are deliberately excluded from discovery/search timelines until a live
-- delivery promotes them.  The delivery marker is separate from provenance so
-- a redelivery can atomically claim one-shot notifications/streaming effects.
ALTER TABLE statuses
    ADD COLUMN ingest_provenance text NOT NULL DEFAULT 'delivery'
        CHECK (ingest_provenance IN ('history', 'explicit_resolution', 'delivery')),
    ADD COLUMN history_fetched_at timestamptz,
    ADD COLUMN history_last_touched_at timestamptz,
    ADD COLUMN delivery_side_effects_at timestamptz;

-- `download_on_demand` also represents an attachment's intrinsic media
-- policy (for example PeerTube HLS).  Keep the history-only reason separate
-- so a later live delivery promotes only media that hydration itself deferred.
ALTER TABLE media_attachments
    ADD COLUMN history_deferred boolean NOT NULL DEFAULT false;

-- Everything predating this feature has already travelled through its normal
-- ingest path.  Mark those rows so replay cannot manufacture notifications.
UPDATE statuses
SET delivery_side_effects_at = created_at
WHERE uri IS NOT NULL;

CREATE INDEX idx_statuses_history_retention
    ON statuses (history_last_touched_at, id)
    WHERE ingest_provenance = 'history' AND deleted_at IS NULL;

-- Search is intentionally a promotion boundary in v1: cold history is useful
-- on its author's profile, not a way to inflate global discovery surfaces.
DROP INDEX idx_statuses_search;
CREATE INDEX idx_statuses_search
    ON statuses USING gin (to_tsvector('simple'::regconfig, content))
    WHERE reblog_of_id IS NULL
      AND ingest_provenance <> 'history';

CREATE TABLE remote_history_settings (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    enabled boolean NOT NULL DEFAULT false,
    retention_days integer NOT NULL DEFAULT 90
        CHECK (retention_days BETWEEN 1 AND 3650),
    bare_iri_enabled boolean NOT NULL DEFAULT false,
    updated_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO remote_history_settings (singleton) VALUES (true);

CREATE TABLE remote_history_states (
    account_id bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    outbox_uri text,
    first_page_uri text,
    next_page_uri text,
    state text NOT NULL DEFAULT 'idle'
        CHECK (state IN ('idle', 'queued', 'fetching', 'partial', 'complete',
                         'unsupported', 'backoff')),
    reported_total_items bigint CHECK (reported_total_items IS NULL OR reported_total_items >= 0),
    outbox_etag text,
    first_page_etag text,
    anonymous_visibility text NOT NULL DEFAULT 'unknown'
        CHECK (anonymous_visibility IN ('unknown', 'allowed', 'restricted')),
    last_attempt_at timestamptz,
    last_success_at timestamptz,
    last_viewed_at timestamptz,
    retry_at timestamptz,
    last_error_class text,
    pages_fetched bigint NOT NULL DEFAULT 0 CHECK (pages_fetched >= 0),
    items_seen bigint NOT NULL DEFAULT 0 CHECK (items_seen >= 0),
    items_accepted bigint NOT NULL DEFAULT 0 CHECK (items_accepted >= 0),
    bytes_fetched bigint NOT NULL DEFAULT 0 CHECK (bytes_fetched >= 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_remote_history_states_retry
    ON remote_history_states (retry_at)
    WHERE state = 'backoff';
CREATE INDEX idx_remote_history_states_viewed
    ON remote_history_states (last_viewed_at);

-- Existing remote actors already have validated collection URLs. Make the
-- feature immediately available after upgrade instead of waiting for every
-- actor to be refreshed once more.
INSERT INTO remote_history_states (account_id, outbox_uri)
SELECT id, outbox_url
FROM accounts
WHERE domain IS NOT NULL AND outbox_url <> '';

-- Only live jobs remain in this table.  Completion removes the row, while a
-- lease timeout makes a crashed attempt claimable again.
CREATE TABLE remote_history_jobs (
    id bigint PRIMARY KEY,
    account_id bigint NOT NULL UNIQUE REFERENCES accounts(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('initial', 'refresh', 'older')),
    page_uri text,
    origin text NOT NULL,
    requested_by bigint REFERENCES accounts(id) ON DELETE SET NULL,
    state text NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'leased')),
    run_at timestamptz NOT NULL DEFAULT now(),
    lease_owner text,
    leased_until timestamptz,
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX idx_remote_history_jobs_due
    ON remote_history_jobs (run_at, created_at)
    WHERE state = 'pending';
CREATE INDEX idx_remote_history_jobs_expired_lease
    ON remote_history_jobs (leased_until)
    WHERE state = 'leased';
CREATE INDEX idx_remote_history_jobs_origin
    ON remote_history_jobs (origin, run_at);

-- A database lease, rather than an in-process mutex, keeps the one-request
-- per-origin invariant true across rolling deploys and multiple server
-- processes. Deleting a completed job releases its origin automatically.
CREATE TABLE remote_history_origin_leases (
    origin text PRIMARY KEY,
    job_id bigint NOT NULL UNIQUE REFERENCES remote_history_jobs(id) ON DELETE CASCADE,
    lease_owner text NOT NULL,
    leased_until timestamptz NOT NULL
);

CREATE INDEX idx_remote_history_origin_leases_expiry
    ON remote_history_origin_leases (leased_until);

-- Durable cycle detection: a hostile collection cannot make "load older"
-- walk the same pages forever across worker restarts.
CREATE TABLE remote_history_pages (
    account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    page_uri text NOT NULL,
    fetched_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, page_uri)
);

CREATE INDEX idx_remote_history_pages_fetched_at
    ON remote_history_pages (fetched_at);
