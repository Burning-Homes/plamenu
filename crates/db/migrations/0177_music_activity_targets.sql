-- Retain the exact local entity a voluntary website response is about.
ALTER TABLE webmention_interactions ADD COLUMN target TEXT
    CHECK(target IS NULL OR length(target)<=2048);
-- Recover historical context only when its source has one unambiguous target.
UPDATE webmention_interactions i SET target=targets.target FROM (
    SELECT source,min(target) AS target FROM webmentions GROUP BY source
    HAVING count(DISTINCT target)=1
) targets WHERE i.source=targets.source;
CREATE INDEX webmention_interactions_target ON webmention_interactions(target,status_id)
    WHERE target IS NOT NULL;
