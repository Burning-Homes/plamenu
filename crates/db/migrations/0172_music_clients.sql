-- Purpose-limited Subsonic credentials. Salted authentication needs recoverable
-- material, encrypted with a purpose-derived operator key and immutable row AAD.
CREATE TABLE music_client_keys (
    id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    label TEXT NOT NULL CHECK(length(label) BETWEEN 1 AND 100),
    token_hash TEXT NOT NULL UNIQUE CHECK(token_hash ~ '^[A-Za-z0-9_-]{43}$'),
    encrypted_token TEXT NOT NULL,
    key_version INTEGER NOT NULL CHECK(key_version>0),
    can_manage BOOLEAN NOT NULL DEFAULT false,
    broadcast_playing BOOLEAN NOT NULL DEFAULT false,
    CHECK(NOT broadcast_playing OR can_manage),
    parent_token_id BIGINT REFERENCES oauth_tokens(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    CHECK(expires_at>created_at)
);
CREATE INDEX music_client_keys_account ON music_client_keys(account_id,id DESC);
CREATE FUNCTION retire_account_music_client_keys() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
        OR (OLD.suspended_at IS NULL AND NEW.suspended_at IS NOT NULL) THEN
        DELETE FROM music_client_keys WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_music_client_keys AFTER UPDATE OF deleted_at,suspended_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_client_keys();
