-- Authorized-fetch servers such as Mastodon conceal a protected object from
-- an unauthorized signer with 404. That denial is signer-specific rather
-- than proof that the resource is globally absent.
DELETE FROM remote_fetch_failures AS global
WHERE global.scope = 'resource'
  AND global.last_error = 'remote answered 404'
  AND EXISTS (
      SELECT 1
      FROM remote_fetch_failures AS scoped
      WHERE scoped.scope = 'resource-instance'
        AND scoped.failure_key = global.failure_key
  );

UPDATE remote_fetch_failures
SET scope = 'resource-instance'
WHERE scope = 'resource'
  AND last_error = 'remote answered 404';
