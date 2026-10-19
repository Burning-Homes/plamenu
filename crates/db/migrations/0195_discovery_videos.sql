-- Video catalogue metadata stays separate from imported statuses and media.
ALTER TABLE discovery_entries DROP CONSTRAINT discovery_entries_kind_check;
ALTER TABLE discovery_entries ADD CHECK (kind IN ('source','profile','post','community','video'));
ALTER TABLE discovery_settings DROP CONSTRAINT discovery_settings_disabled_mechanisms_check;
ALTER TABLE discovery_settings ADD CHECK (
    disabled_mechanisms <@ ARRAY['source_inspection','peer_exploration','directory',
        'community_directory','video_catalogue','video_indexing','outbox','metadata_cache',
        'profile_listing','public_post_indexing','recommendations','peer_sharing','wider_search']::text[]
    AND cardinality(disabled_mechanisms)<=13 AND array_position(disabled_mechanisms,NULL) IS NULL);
ALTER TABLE discovery_sources DROP CONSTRAINT discovery_sources_capabilities_check;
ALTER TABLE discovery_sources ADD CHECK (
    capabilities <@ ARRAY['source_inspection','peer_exploration','directory','community_directory','video_catalogue','outbox']::text[]
    AND cardinality(capabilities)<=6 AND array_position(capabilities,NULL) IS NULL);
-- Candidate identities fence actor updates before their first verification.
-- Each row is bounded by an existing job; directory text is never retained.
CREATE TABLE discovery_video_work (
    job_id bigint PRIMARY KEY REFERENCES discovery_jobs(id) ON DELETE CASCADE,
    uuid text NOT NULL CHECK (length(uuid)=36),
    owner_uri text NOT NULL CHECK (octet_length(owner_uri) BETWEEN 1 AND 2048),
    channel_uri text NOT NULL CHECK (octet_length(channel_uri) BETWEEN 1 AND 2048)
);
CREATE TABLE discovery_video_details (
    entry_id bigint PRIMARY KEY REFERENCES discovery_entries(id) ON DELETE CASCADE,
    uuid text NOT NULL CHECK (length(uuid)=36),
    owner_uri text NOT NULL CHECK (octet_length(owner_uri) BETWEEN 1 AND 2048),
    channel_uri text NOT NULL CHECK (octet_length(channel_uri) BETWEEN 1 AND 2048),
    channel_title text CHECK (octet_length(channel_title)<=1024),
    duration integer CHECK (duration BETWEEN 0 AND 604800)
);
CREATE INDEX discovery_video_owner ON discovery_video_details(owner_uri);
CREATE INDEX discovery_video_channel ON discovery_video_details(channel_uri);
CREATE FUNCTION discovery_clear_video_text() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.kind='video' AND NEW.state<>'active' THEN
        UPDATE discovery_video_details SET channel_title=NULL,duration=NULL WHERE entry_id=NEW.id;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER discovery_clear_video_text AFTER UPDATE ON discovery_entries
    FOR EACH ROW EXECUTE FUNCTION discovery_clear_video_text();
UPDATE discovery_settings SET revision=revision+1;
UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL;
