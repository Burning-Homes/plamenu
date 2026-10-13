-- Listening is an explicit authenticated event, not an audio-byte request.
CREATE TABLE music_listening_preferences (
    account_id BIGINT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    history_enabled BOOLEAN NOT NULL DEFAULT true,
    minimum_seconds INTEGER NOT NULL DEFAULT 30 CHECK (minimum_seconds BETWEEN 1 AND 600),
    minimum_percent INTEGER NOT NULL DEFAULT 50 CHECK (minimum_percent BETWEEN 1 AND 100),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision > 0)
);
CREATE TABLE music_listen_sessions (
    id BIGINT PRIMARY KEY,
    token TEXT NOT NULL UNIQUE CHECK (token ~ '^[a-f0-9]{64}$'),
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    request_key TEXT NOT NULL CHECK (request_key ~ '^[a-zA-Z0-9_-]{1,64}$'),
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE CASCADE,
    status_id BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    duration DOUBLE PRECISION NOT NULL CHECK (duration >= 0 AND duration < 'Infinity'::float8),
    threshold_seconds DOUBLE PRECISION NOT NULL CHECK (threshold_seconds > 0 AND threshold_seconds < 'Infinity'::float8),
    started_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    last_report_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    reported_seconds DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (reported_seconds >= 0 AND reported_seconds < 'Infinity'::float8),
    played_seconds DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (played_seconds >= 0 AND played_seconds < 'Infinity'::float8),
    counted_at TIMESTAMPTZ,
    history_id BIGINT UNIQUE,
    finished_at TIMESTAMPTZ,
    CHECK (num_nonnulls(asset_id,status_id)=1),
    CHECK ((counted_at IS NULL) = (history_id IS NULL)),
    UNIQUE (account_id,request_key)
);
CREATE INDEX music_listen_sessions_account ON music_listen_sessions(account_id,history_id DESC) WHERE counted_at IS NOT NULL;
CREATE INDEX music_listen_sessions_period ON music_listen_sessions(account_id,counted_at) WHERE counted_at IS NOT NULL;
CREATE INDEX music_listen_sessions_pending ON music_listen_sessions(account_id,last_report_at) WHERE counted_at IS NULL;
CREATE INDEX music_listen_sessions_asset ON music_listen_sessions(asset_id) WHERE asset_id IS NOT NULL;
CREATE INDEX music_listen_sessions_status ON music_listen_sessions(status_id) WHERE status_id IS NOT NULL;

CREATE FUNCTION retire_account_music_listening() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        PERFORM pg_advisory_xact_lock(hashtextextended('music-owner:' || NEW.id::text,0));
        DELETE FROM music_queues WHERE account_id=NEW.id;
        DELETE FROM music_listen_sessions WHERE account_id=NEW.id;
        DELETE FROM music_listening_preferences WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_music_listening AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_listening();
