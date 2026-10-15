-- Exact actor fetching used to reject BookWyrm's <actor>/#main-key alias
-- because the returned canonical ID omits its final slash. The signer
-- resolver now verifies the canonical document. Allow affected installations
-- to retry, including resources whose old finite budget was exhausted.
-- Preserve every other failure and every authorization-specific scope.
DELETE FROM remote_fetch_failures
WHERE scope = 'resource'
  AND right(failure_key, 1) = '/'
  AND last_error = 'invalid actor document: actor id '
      || left(failure_key, -1)
      || ' does not match fetched uri ' || failure_key;
