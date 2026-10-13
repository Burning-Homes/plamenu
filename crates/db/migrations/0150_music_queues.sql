-- Queue membership retains references, never media permissions or metadata.
CREATE TABLE music_queues (
    id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    device_key TEXT NOT NULL CHECK (device_key ~ '^[a-zA-Z0-9_-]{1,64}$'),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0),
    current_entry_id BIGINT,
    position_seconds DOUBLE PRECISION NOT NULL DEFAULT 0
        CHECK (position_seconds >= 0 AND position_seconds < 'Infinity'::float8),
    repeat_mode TEXT NOT NULL DEFAULT 'off' CHECK (repeat_mode IN ('off','one','all')),
    shuffle_seed TEXT CHECK (shuffle_seed ~ '^[a-f0-9]{32}$'),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (account_id,device_key)
);
CREATE TABLE music_queue_entries (
    id BIGINT PRIMARY KEY,
    queue_id BIGINT NOT NULL REFERENCES music_queues(id) ON DELETE CASCADE,
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE CASCADE,
    status_id BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position BETWEEN 1 AND 1000),
    CHECK (num_nonnulls(asset_id,status_id)=1),
    UNIQUE (id,queue_id),
    UNIQUE (queue_id,position) DEFERRABLE INITIALLY DEFERRED
);
ALTER TABLE music_queues ADD CONSTRAINT music_queue_current_entry
    FOREIGN KEY (current_entry_id,id) REFERENCES music_queue_entries(id,queue_id)
    ON DELETE SET NULL (current_entry_id) DEFERRABLE INITIALLY DEFERRED;
CREATE INDEX music_queue_entries_asset ON music_queue_entries(asset_id) WHERE asset_id IS NOT NULL;
CREATE INDEX music_queue_entries_status ON music_queue_entries(status_id) WHERE status_id IS NOT NULL;
