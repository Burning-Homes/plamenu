-- Repair remote accounts stored while `host_of` only understood https://.
--
-- A hidden-service actor's refresh (any inbound activity triggers one when
-- the signer cache misses) derived its domain from the http:// actor id,
-- got nothing, and overwrote the correct webfinger-derived value with ''.
-- Handles then rendered as "@nina@" and every handle-based flow downstream
-- (mentions, reply prefill, acct dedup) degraded with them.
--
-- Scope: exactly the rows that bug could produce — remote rows (a local
-- account's domain is NULL, never '') whose actor id is a hidden-service
-- http:// URI. The domain becomes the URI's host[:port], the same value the
-- fixed host_of derives, so re-ingest converges instead of flip-flopping.
UPDATE accounts
   SET domain = substring(uri FROM '^https?://([^/?#]+)')
 WHERE domain = ''
   AND uri ~ '^http://[^/?#]+\.(onion|i2p)([/:?#]|$)';
