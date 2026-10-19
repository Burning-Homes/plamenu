-- Reserve before sending, reconcile only fully read responses. Lost workers
-- retain their full charge for 24 hours and their network slot for 30 seconds.
ALTER TABLE discovery_settings
    ADD COLUMN requests_per_day bigint NOT NULL DEFAULT 500 CHECK (requests_per_day BETWEEN 1 AND 100000),
    ADD COLUMN requests_per_origin bigint NOT NULL DEFAULT 50 CHECK (requests_per_origin BETWEEN 1 AND 100000),
    ADD COLUMN bytes_per_day bigint NOT NULL DEFAULT 67108864 CHECK (bytes_per_day BETWEEN 2097152 AND 107374182400);
CREATE TABLE discovery_requests (
    id bigint PRIMARY KEY,
    origin text NOT NULL CHECK (octet_length(origin) BETWEEN 1 AND 512),
    started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    active_until timestamptz NOT NULL DEFAULT clock_timestamp() + interval '30 seconds',
    bytes bigint NOT NULL DEFAULT 2097152 CHECK (bytes BETWEEN 0 AND 2097152),
    completed boolean NOT NULL DEFAULT false
);
CREATE INDEX discovery_requests_window ON discovery_requests(started_at);
CREATE INDEX discovery_requests_origin ON discovery_requests(origin, active_until);
ALTER TABLE discovery_sources ADD COLUMN last_success_at timestamptz;
ALTER TABLE discovery_sources ADD COLUMN last_failure_at timestamptz;
