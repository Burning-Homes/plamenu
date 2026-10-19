-- Pacing may finish already-started verification chains before waiting for the
-- next token. Wire-count, origin, body and concurrency limits still apply.
ALTER TABLE discovery_requests ADD COLUMN lease_token bigint;
CREATE INDEX discovery_requests_lease ON discovery_requests(lease_token);
