-- Persistent native HTTP destinations are independent of browser connections.
CREATE TABLE music_outputs (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mount TEXT NOT NULL UNIQUE CHECK (mount ~ '^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$'),
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    description TEXT NOT NULL DEFAULT '' CHECK (char_length(description)<=4000),
    genre TEXT NOT NULL DEFAULT '' CHECK (char_length(genre)<=200),
    format TEXT NOT NULL CHECK (format IN ('mp3','ogg','opus')),
    bitrate INTEGER NOT NULL CHECK (bitrate BETWEEN 32 AND 512),
    sample_rate INTEGER NOT NULL DEFAULT 44100 CHECK (sample_rate IN (22050,24000,32000,44100,48000)),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','public')),
    listen_token_hash TEXT,
    enabled BOOLEAN NOT NULL DEFAULT true,
    transport TEXT NOT NULL DEFAULT 'paused' CHECK (transport IN ('playing','paused','stopped')),
    device TEXT NOT NULL CHECK (char_length(device) BETWEEN 1 AND 64),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision>0),
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation>0),
    worker_lease BIGINT,
    lease_until TIMESTAMPTZ,
    current_entry_id BIGINT,
    position_seconds DOUBLE PRECISION NOT NULL DEFAULT 0 CHECK (position_seconds>=0 AND position_seconds<'Infinity'::float8),
    last_error TEXT CHECK (char_length(last_error)<=2000),
    now_playing TEXT NOT NULL DEFAULT '' CHECK (char_length(now_playing)<=1000),
    codec_header BYTEA NOT NULL DEFAULT ''::bytea CHECK (octet_length(codec_header)<=262144),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((visibility='private')=(listen_token_hash IS NOT NULL)),
    CHECK ((worker_lease IS NULL)=(lease_until IS NULL))
);
CREATE INDEX music_outputs_owner ON music_outputs(owner_id,id DESC);
CREATE INDEX music_outputs_live ON music_outputs(id) WHERE enabled AND transport='playing';
CREATE TABLE music_output_listeners (
    id BIGINT PRIMARY KEY,
    output_id BIGINT NOT NULL REFERENCES music_outputs(id) ON DELETE CASCADE,
    generation BIGINT NOT NULL,
    lease_until TIMESTAMPTZ NOT NULL
);
CREATE INDEX music_output_listeners_output ON music_output_listeners(output_id,lease_until);
CREATE TABLE music_output_chunks (
    output_id BIGINT NOT NULL REFERENCES music_outputs(id) ON DELETE CASCADE,
    generation BIGINT NOT NULL,
    sequence BIGINT NOT NULL,
    title TEXT NOT NULL DEFAULT '' CHECK (char_length(title)<=1000),
    bytes BYTEA NOT NULL CHECK (octet_length(bytes) BETWEEN 1 AND 16384),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (output_id,generation,sequence)
);
CREATE FUNCTION retire_account_music_outputs() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_outputs WHERE owner_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_music_outputs AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_outputs();
