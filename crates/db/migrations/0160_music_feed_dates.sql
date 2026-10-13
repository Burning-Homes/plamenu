-- Legacy entries did not retain their addition dates: the first feed adoption
-- timestamp is explicit rather than inventing a historical addition time.
ALTER TABLE music_collection_entries ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
ALTER TABLE music_assets ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE FUNCTION music_asset_updated() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.revision IS DISTINCT FROM OLD.revision THEN
        NEW.updated_at=now();
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER music_asset_updated BEFORE UPDATE ON music_assets
FOR EACH ROW EXECUTE FUNCTION music_asset_updated();

ALTER TABLE music_podcast_episodes ADD COLUMN updated_at TIMESTAMPTZ NOT NULL DEFAULT now();
CREATE FUNCTION music_podcast_episode_updated() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (NEW.title,NEW.description,NEW.link,NEW.image_url,NEW.audio_url,
        NEW.audio_type,NEW.audio_length,NEW.duration_seconds,NEW.published_at,
        NEW.season_number,NEW.episode_number,NEW.episode_type)
        IS DISTINCT FROM
       (OLD.title,OLD.description,OLD.link,OLD.image_url,OLD.audio_url,
        OLD.audio_type,OLD.audio_length,OLD.duration_seconds,OLD.published_at,
        OLD.season_number,OLD.episode_number,OLD.episode_type) THEN
        NEW.updated_at=now();
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER music_podcast_episode_updated BEFORE UPDATE ON music_podcast_episodes
FOR EACH ROW EXECUTE FUNCTION music_podcast_episode_updated();
