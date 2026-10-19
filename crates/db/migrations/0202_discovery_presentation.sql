-- Presentation metadata is independent of consent and retention decisions.
CREATE TABLE discovery_actor_labels (
    uri text PRIMARY KEY,
    username text NOT NULL CHECK(octet_length(username) BETWEEN 1 AND 255),
    updated_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO discovery_actor_labels(uri,username)
SELECT uri,username FROM accounts WHERE uri IN (
 SELECT actor_uri FROM discovery_entries UNION SELECT channel_uri FROM discovery_video_details
) AND octet_length(username) BETWEEN 1 AND 255 ON CONFLICT DO NOTHING;
INSERT INTO discovery_actor_labels(uri,username)
SELECT uri,document->>'preferredUsername' FROM discovery_actor_evidence
WHERE octet_length(document->>'preferredUsername') BETWEEN 1 AND 255 ON CONFLICT DO NOTHING;
-- Older actors need one paced canonical refresh to recover discarded usernames.
UPDATE discovery_identity i SET next_check_at=now(),hint_changed=true
WHERE outcome='active' AND EXISTS(SELECT 1 FROM discovery_entries e WHERE e.canonical_uri=i.canonical_uri AND e.kind IN ('profile','community'))
AND NOT EXISTS(SELECT 1 FROM discovery_actor_labels l WHERE l.uri=i.canonical_uri);
CREATE TABLE discovery_server_icons (
 source_id bigint PRIMARY KEY REFERENCES discovery_sources(id) ON DELETE CASCADE,
 icon_uri text CHECK(octet_length(icon_uri)<=2048),
 image bytea CHECK(octet_length(image)<=262144),
 next_check_at timestamptz NOT NULL DEFAULT now(),
 outcome text NOT NULL DEFAULT 'pending'
);
CREATE TABLE discovery_page_receipts (
 token text PRIMARY KEY,
 account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
 entries bigint[] NOT NULL CHECK(cardinality(entries)<=2000),
 created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX discovery_page_receipts_owner ON discovery_page_receipts(account_id,created_at DESC);
