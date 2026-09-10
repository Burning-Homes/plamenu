-- Keep lineage total for maintenance jobs, extensions, and older integrations
-- that insert directly into custom_emojis without knowing about origin_id.
-- Application writes provide an origin explicitly; this is the compatibility
-- and integrity backstop.
CREATE FUNCTION custom_emojis_assign_origin() RETURNS trigger AS $$
BEGIN
    IF NEW.origin_id IS NULL THEN
        IF NEW.domain IS NOT NULL AND NEW.uri IS NOT NULL THEN
            SELECT id INTO NEW.origin_id
            FROM custom_emoji_origins
            WHERE canonical_uri = NEW.uri
            ORDER BY id
            LIMIT 1;
        END IF;

        IF NEW.origin_id IS NULL THEN
            NEW.origin_id := NEW.id;
            INSERT INTO custom_emoji_origins (id, source, canonical_uri)
            VALUES (
                NEW.id,
                CASE WHEN NEW.domain IS NULL THEN 'local' ELSE 'federated' END,
                CASE WHEN NEW.domain IS NULL THEN NULL ELSE NEW.uri END
            )
            ON CONFLICT (id) DO NOTHING;
        END IF;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER custom_emojis_assign_origin_before_insert
BEFORE INSERT ON custom_emojis
FOR EACH ROW EXECUTE FUNCTION custom_emojis_assign_origin();
