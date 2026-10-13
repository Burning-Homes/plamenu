-- RSS producers are shared feed identities, never ActivityPub accounts.
CREATE TABLE music_podcasts (
    id BIGINT PRIMARY KEY,
    feed_url TEXT NOT NULL UNIQUE CHECK(length(feed_url) BETWEEN 1 AND 2048),
    title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 512),
    description TEXT NOT NULL DEFAULT '' CHECK(length(description)<=600000),
    author TEXT NOT NULL DEFAULT '' CHECK(length(author)<=512),
    link TEXT,
    image_url TEXT,
    language TEXT NOT NULL DEFAULT '' CHECK(length(language)<=32),
    categories TEXT[] NOT NULL DEFAULT '{}',
    explicit BOOLEAN NOT NULL DEFAULT false,
    etag TEXT,
    last_modified TEXT,
    last_fetched_at TIMESTAMPTZ,
    last_error TEXT,
    next_refresh_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    refresh_lease BIGINT,
    refresh_until TIMESTAMPTZ,
    revision BIGINT NOT NULL DEFAULT 1 CHECK(revision>0)
);
CREATE INDEX music_podcasts_due ON music_podcasts(next_refresh_at,id);
CREATE TABLE music_podcast_episodes (
    id BIGINT PRIMARY KEY,
    podcast_id BIGINT NOT NULL REFERENCES music_podcasts(id) ON DELETE CASCADE,
    guid TEXT NOT NULL CHECK(length(guid) BETWEEN 1 AND 2048),
    title TEXT NOT NULL CHECK(length(title) BETWEEN 1 AND 512),
    description TEXT NOT NULL DEFAULT '' CHECK(length(description)<=600000),
    link TEXT,
    image_url TEXT,
    audio_url TEXT NOT NULL CHECK(length(audio_url) BETWEEN 1 AND 2048),
    audio_type TEXT NOT NULL DEFAULT '' CHECK(length(audio_type)<=128),
    audio_length BIGINT CHECK(audio_length>=0),
    duration_seconds INTEGER CHECK(duration_seconds>=0),
    published_at TIMESTAMPTZ,
    season_number INTEGER CHECK(season_number>=0),
    episode_number INTEGER CHECK(episode_number>=0),
    episode_type TEXT CHECK(episode_type IN ('full','trailer','bonus')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE(podcast_id,guid)
);
CREATE INDEX music_podcast_episode_order ON music_podcast_episodes(podcast_id,published_at DESC NULLS LAST,id DESC);
CREATE TABLE music_podcast_subscriptions (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    podcast_id BIGINT NOT NULL REFERENCES music_podcasts(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(account_id,podcast_id)
);
CREATE INDEX music_podcast_subscribed ON music_podcast_subscriptions(podcast_id);
CREATE TABLE music_podcast_plays (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    episode_id BIGINT NOT NULL REFERENCES music_podcast_episodes(id) ON DELETE CASCADE,
    played_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(account_id,episode_id)
);
CREATE FUNCTION retire_music_podcast_account() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_podcast_subscriptions WHERE account_id=NEW.id;
        DELETE FROM music_podcast_plays WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER music_podcast_account_retirement AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_music_podcast_account();
