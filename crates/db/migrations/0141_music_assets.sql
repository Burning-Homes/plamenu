-- Durable music is independent of a social publication and uses a reserved
-- storage namespace. Collection membership never changes its authorization.
CREATE TABLE music_artists (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    UNIQUE(owner_id,name)
);
CREATE TABLE music_albums (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    artist_id BIGINT NOT NULL REFERENCES music_artists(id),
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    release_year INTEGER CHECK (release_year BETWEEN 1 AND 9999),
    UNIQUE NULLS NOT DISTINCT(owner_id,artist_id,name,release_year)
);
CREATE TABLE music_assets (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    title TEXT NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
    artist_id BIGINT NOT NULL REFERENCES music_artists(id),
    album_id BIGINT REFERENCES music_albums(id),
    genre TEXT NOT NULL DEFAULT '' CHECK (char_length(genre)<=200),
    track_number INTEGER CHECK (track_number BETWEEN 1 AND 9999),
    disc_number INTEGER CHECK (disc_number BETWEEN 1 AND 9999),
    musicbrainz_id TEXT CHECK (musicbrainz_id ~* '^[a-f0-9]{8}(-[a-f0-9]{4}){3}-[a-f0-9]{12}$'),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('public','local','private')),
    original_file TEXT NOT NULL UNIQUE CHECK (original_file ~ '^music[0-9]+\.orig$'),
    stream_file TEXT NOT NULL UNIQUE CHECK (stream_file ~ '^music[0-9]+\.mp3$'),
    original_format TEXT NOT NULL CHECK (original_format IN ('mp3','flac','wav','aac','m4a','ogg','mka','wma')),
    original_bytes BIGINT NOT NULL CHECK (original_bytes>0),
    stream_bytes BIGINT NOT NULL CHECK (stream_bytes>0),
    audio_hash TEXT NOT NULL CHECK (audio_hash ~ '^[a-f0-9]{64}$'),
    duration DOUBLE PRECISION NOT NULL CHECK (duration>0 AND duration<'Infinity'::float8),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision>0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(owner_id,audio_hash)
);
CREATE INDEX music_assets_catalogue ON music_assets(id DESC) WHERE visibility='public';
CREATE INDEX music_assets_owner ON music_assets(owner_id,id DESC);
CREATE TABLE music_asset_grants (
    asset_id BIGINT NOT NULL REFERENCES music_assets(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    PRIMARY KEY(asset_id,account_id)
);

-- Failed/cancelled writes and retired assets keep their quota debt until the
-- existing durable cleanup worker acknowledges physical removal.
CREATE TABLE music_storage_debt (
    file_name TEXT PRIMARY KEY REFERENCES media_cleanup_jobs(file_name) ON DELETE CASCADE,
    owner_id BIGINT REFERENCES accounts(id) ON DELETE SET NULL,
    size_bytes BIGINT NOT NULL CHECK (size_bytes>0)
);
