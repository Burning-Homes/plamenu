-- Authorization failures depend on the HTTP-signature actor. Historical
-- instance-signed 401/403 rows must not suppress a newly recipient-signed
-- attempt for the same protected ActivityPub object.
UPDATE remote_fetch_failures
SET scope = 'resource-instance'
WHERE scope = 'resource'
  AND last_error IN ('remote answered 401', 'remote answered 403');
