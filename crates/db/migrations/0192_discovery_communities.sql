-- Communities retain actor metadata only; they never seed post/outbox indexing.
ALTER TABLE discovery_entries DROP CONSTRAINT discovery_entries_kind_check;
ALTER TABLE discovery_entries ADD CHECK (kind IN ('source','profile','post','community'));
ALTER TABLE discovery_settings DROP CONSTRAINT discovery_settings_disabled_mechanisms_check;
ALTER TABLE discovery_settings ADD CHECK (
    disabled_mechanisms <@ ARRAY['source_inspection','peer_exploration','directory',
        'community_directory','outbox','metadata_cache','profile_listing',
        'public_post_indexing','recommendations','peer_sharing']::text[]
    AND cardinality(disabled_mechanisms)<=10 AND array_position(disabled_mechanisms,NULL) IS NULL);
ALTER TABLE discovery_sources DROP CONSTRAINT discovery_sources_capabilities_check;
ALTER TABLE discovery_sources ADD CHECK (
    capabilities <@ ARRAY['source_inspection','peer_exploration','directory','community_directory','outbox']::text[]
    AND cardinality(capabilities)<=5 AND array_position(capabilities,NULL) IS NULL);
ALTER TABLE discovery_jobs DROP CONSTRAINT discovery_jobs_operation_check;
ALTER TABLE discovery_jobs ADD CHECK (operation IN ('inspect_source','list_peers','list_directory',
    'list_communities','sample_outbox','refresh_entry','revalidate_entry'));
UPDATE discovery_settings SET revision=revision+1;
UPDATE discovery_jobs SET lease_token=NULL,lease_owner=NULL,leased_until=NULL;
