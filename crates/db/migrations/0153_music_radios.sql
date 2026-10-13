-- Owned station configuration is independent from playback outputs.
CREATE TABLE music_radios (
    id BIGINT PRIMARY KEY,
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    description TEXT NOT NULL DEFAULT '' CHECK (char_length(description)<=4000),
    config TEXT NOT NULL DEFAULT '' CHECK (char_length(config)<=50000),
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private','local','public')),
    revision BIGINT NOT NULL DEFAULT 1 CHECK (revision>0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX music_radios_owner ON music_radios(owner_id,id DESC);
CREATE INDEX music_radios_public ON music_radios(id DESC) WHERE visibility='public';
CREATE TABLE music_radio_grants (
    radio_id BIGINT NOT NULL REFERENCES music_radios(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    PRIMARY KEY(radio_id,account_id)
);
CREATE INDEX music_radio_grants_account ON music_radio_grants(account_id);
CREATE FUNCTION retire_account_music_radios() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL THEN
        DELETE FROM music_radios WHERE owner_id=NEW.id;
        DELETE FROM music_radio_grants WHERE account_id=NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER retire_account_music_radios AFTER UPDATE OF deleted_at ON accounts
    FOR EACH ROW EXECUTE FUNCTION retire_account_music_radios();
