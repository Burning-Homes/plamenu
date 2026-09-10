-- Webxdc storage lifecycle: bounded retained state, activity timestamps, and
-- durable deletion markers. A tombstoned session keeps only its small actor
-- row so already-queued Delete deliveries can continue to use the actor's
-- signing keys; its package, log, memberships, and guest credentials are
-- removed immediately by the application transaction.
ALTER TABLE webxdc_sessions
    ADD COLUMN last_activity_at timestamptz,
    ADD COLUMN storage_bytes bigint;

UPDATE webxdc_sessions s
SET last_activity_at = greatest(
        s.published_at,
        coalesce(
            (SELECT max(u.created_at) FROM webxdc_updates u WHERE u.session_id = s.id),
            s.published_at
        )
    ),
    storage_bytes = octet_length(s.bundle_bytes)::bigint
        + coalesce((
            SELECT sum(octet_length(f.bytes))::bigint
            FROM webxdc_files f
            WHERE f.session_id = s.id
        ), 0)
        + coalesce((
            SELECT sum(
                octet_length(u.raw_create::text)
                + octet_length(u.webxdc_update::text)
            )::bigint
            FROM webxdc_updates u
            WHERE u.session_id = s.id
        ), 0);

ALTER TABLE webxdc_sessions
    ALTER COLUMN last_activity_at SET DEFAULT now(),
    ALTER COLUMN last_activity_at SET NOT NULL,
    ALTER COLUMN storage_bytes SET DEFAULT 0,
    ALTER COLUMN storage_bytes SET NOT NULL,
    ADD CHECK (storage_bytes >= 0);

CREATE INDEX idx_webxdc_sessions_lifecycle
    ON webxdc_sessions (ended_at, last_activity_at);

CREATE INDEX idx_webxdc_sessions_creator_storage
    ON webxdc_sessions (creator_account_id)
    WHERE creator_account_id IS NOT NULL;

CREATE TABLE webxdc_tombstones (
    session_id bigint PRIMARY KEY REFERENCES webxdc_sessions (id) ON DELETE CASCADE,
    coordinator_uri text NOT NULL UNIQUE,
    deleted_at timestamptz NOT NULL DEFAULT now()
);
