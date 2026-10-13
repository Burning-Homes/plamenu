-- Music saves retain canonical sources independently of social Like activities.
CREATE TABLE music_favourites (
    id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE CASCADE,
    status_id BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    notification_id BIGINT UNIQUE REFERENCES notifications(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(asset_id,status_id)=1),
    UNIQUE(account_id,asset_id),
    UNIQUE(account_id,status_id)
);
CREATE INDEX music_favourites_account ON music_favourites(account_id,id DESC);
CREATE INDEX music_favourites_asset ON music_favourites(asset_id) WHERE asset_id IS NOT NULL;
CREATE INDEX music_favourites_status ON music_favourites(status_id) WHERE status_id IS NOT NULL;

-- Undo, source deletion and account deletion retract the associated event.
CREATE FUNCTION retract_music_favourite_notification() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM notifications WHERE id=OLD.notification_id;
    RETURN OLD;
END;
$$;
CREATE TRIGGER retract_music_favourite_notification AFTER DELETE ON music_favourites
    FOR EACH ROW EXECUTE FUNCTION retract_music_favourite_notification();
CREATE FUNCTION retire_account_music_favourites() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        PERFORM pg_advisory_xact_lock(hashtextextended('music-owner:' || NEW.id::text,0));
        DELETE FROM music_favourites WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_music_favourites AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_favourites();
