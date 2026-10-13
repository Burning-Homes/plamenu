-- Now-playing reports are explicit client events; requests for bytes never write them.
CREATE TABLE music_client_playing (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    credential_id BIGINT REFERENCES music_client_keys(id) ON DELETE CASCADE,
    asset_id BIGINT REFERENCES music_assets(id) ON DELETE CASCADE,
    status_id BIGINT REFERENCES statuses(id) ON DELETE CASCADE,
    remote_id BIGINT REFERENCES music_remote_resources(id) ON DELETE CASCADE,
    source_key TEXT GENERATED ALWAYS AS (COALESCE('asset-' || asset_id::text,'status-' || status_id::text,'remote-' || remote_id::text)) STORED,
    player TEXT NOT NULL DEFAULT '' CHECK(length(player)<=100),
    reported_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK(num_nonnulls(asset_id,status_id,remote_id)=1),
    PRIMARY KEY(account_id,source_key)
);
CREATE INDEX music_client_playing_reported ON music_client_playing(reported_at DESC,account_id,source_key);

CREATE FUNCTION retire_account_music_client_playing() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL) OR (OLD.suspended_at IS NULL AND NEW.suspended_at IS NOT NULL) THEN
        DELETE FROM music_client_playing WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_music_client_playing AFTER UPDATE OF deleted_at,suspended_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_client_playing();
