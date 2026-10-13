-- Music resource follows are independent of social follows. They confer no
-- access to the owner's follower-only posts or private media.
CREATE TABLE music_follows (
    follower_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    target_uri TEXT NOT NULL CHECK (char_length(target_uri) <= 2048),
    target_kind TEXT NOT NULL CHECK (target_kind IN ('Library','Audio','Track','Artist','Album')),
    target_name TEXT NOT NULL CHECK (char_length(target_name) <= 200),
    local_target BOOLEAN NOT NULL,
    follow_uri TEXT NOT NULL CHECK (char_length(follow_uri) <= 2048),
    pending BOOLEAN NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(follower_id,target_uri),
    UNIQUE(follow_uri)
);
CREATE INDEX music_follows_target ON music_follows(target_uri) WHERE NOT pending;
CREATE INDEX music_follows_owner ON music_follows(owner_id) WHERE local_target;
