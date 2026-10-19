-- Branding has its own durable maintenance job; quiet servers must not wait
-- for an unrelated content job, and icon repair must not resample directories.
ALTER TABLE discovery_jobs DROP CONSTRAINT discovery_jobs_operation_check;
ALTER TABLE discovery_jobs ADD CHECK(operation IN ('inspect_source','list_peers','list_directory',
 'list_communities','sample_outbox','refresh_entry','revalidate_entry','search_source','refresh_presentation'));
