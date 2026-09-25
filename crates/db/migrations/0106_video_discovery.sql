-- Keep public Video browsing and title/description search on bounded indexes.
-- The partial scope agrees with the HTML discovery query; unlisted and
-- history-only imports must not enter a public catalogue.
CREATE INDEX video_discovery_newest ON statuses (id DESC)
WHERE object_type = 'Video' AND visibility = 'public'
  AND reblog_of_id IS NULL AND deleted_at IS NULL
  AND ingest_provenance <> 'history';

CREATE INDEX video_discovery_search ON statuses USING gin (
  to_tsvector('simple', coalesce(title, '') || ' ' || content)
)
WHERE object_type = 'Video' AND visibility = 'public'
  AND reblog_of_id IS NULL AND deleted_at IS NULL
  AND ingest_provenance <> 'history';
