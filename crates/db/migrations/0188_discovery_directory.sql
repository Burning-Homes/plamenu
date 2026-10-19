-- A recheck is an explicit operator request for a known canonical identity.
-- Ordinary directory hints can never clear a withdrawal guard.
ALTER TABLE discovery_jobs DROP CONSTRAINT discovery_jobs_operation_check;
ALTER TABLE discovery_jobs ADD CONSTRAINT discovery_jobs_operation_check
    CHECK (operation IN ('inspect_source','list_peers','list_directory','sample_outbox','refresh_entry','revalidate_entry'));
