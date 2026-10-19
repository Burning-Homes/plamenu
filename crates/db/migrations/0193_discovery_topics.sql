-- Explicit tags inherit their canonical entry's authority and freshness. A
-- new verification replaces the set atomically; revocation clears it too.
CREATE TABLE discovery_topics (
    entry_id bigint NOT NULL REFERENCES discovery_entries(id) ON DELETE CASCADE,
    topic text NOT NULL CHECK (octet_length(topic) BETWEEN 1 AND 256),
    PRIMARY KEY (entry_id, topic)
);
CREATE INDEX discovery_topics_topic ON discovery_topics(topic, entry_id);

CREATE FUNCTION discovery_clear_topics() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.revision <> OLD.revision OR NEW.state <> 'active'
       OR NEW.title IS NULL OR NEW.summary IS NULL THEN
        DELETE FROM discovery_topics WHERE entry_id = NEW.id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER discovery_clear_topics AFTER UPDATE ON discovery_entries
FOR EACH ROW EXECUTE FUNCTION discovery_clear_topics();

-- Explicit, private bookmarks. No browsing history or inferred interests.
CREATE TABLE discovery_lenses (
    account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    topic text NOT NULL CHECK (octet_length(topic) BETWEEN 1 AND 256),
    PRIMARY KEY (account_id, topic)
);
