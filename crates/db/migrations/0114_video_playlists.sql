-- Ordered Video collections are separate from featured-account collections.
-- Entry identity survives reorder, duplicate Videos and deleted Video rows.
CREATE TABLE video_playlists (
    id bigint PRIMARY KEY,
    uuid text NOT NULL UNIQUE DEFAULT gen_random_uuid()::text,
    creator_id bigint NOT NULL REFERENCES accounts(id),
    channel_id bigint NOT NULL REFERENCES accounts(id),
    remote_uri text UNIQUE,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 120),
    description text NOT NULL DEFAULT '' CHECK (char_length(description) <= 4000),
    visibility text NOT NULL CHECK (visibility IN ('public','unlisted','private')),
    revision bigint NOT NULL DEFAULT 1,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    deleted_at timestamptz
);
CREATE INDEX video_playlists_channel ON video_playlists(channel_id,id DESC) WHERE deleted_at IS NULL;
CREATE INDEX video_playlists_creator ON video_playlists(creator_id,id DESC) WHERE deleted_at IS NULL;
CREATE TABLE video_playlist_entries (
    id bigint PRIMARY KEY,
    playlist_id bigint NOT NULL REFERENCES video_playlists(id) ON DELETE CASCADE,
    remote_uri text,
    status_id bigint REFERENCES statuses(id) ON DELETE SET NULL,
    video_uri text NOT NULL,
    position integer NOT NULL CHECK (position BETWEEN 1 AND 1000),
    start_seconds integer NOT NULL DEFAULT 0 CHECK (start_seconds BETWEEN 0 AND 604800),
    stop_seconds integer CHECK (stop_seconds > start_seconds AND stop_seconds <= 604800),
    UNIQUE(playlist_id, remote_uri)
);
CREATE INDEX video_playlist_entries_order ON video_playlist_entries(playlist_id,position,id);
-- A tombstone for an unknown Playlist Delete fences a delayed Create.
CREATE TABLE video_playlist_tombstones (
    uri text PRIMARY KEY,
    actor_uri text NOT NULL,
    deleted_at timestamptz NOT NULL DEFAULT now()
);
