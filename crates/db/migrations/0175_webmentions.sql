-- Website identity remains separate from ActivityPub accounts and statuses.
CREATE TABLE webmentions (
    id BIGINT PRIMARY KEY,
    source TEXT NOT NULL CHECK(length(source)<=2048),
    target TEXT NOT NULL CHECK(length(target)<=2048),
    owner_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    entry JSONB,
    photo_bytes BYTEA CHECK(octet_length(photo_bytes)<=131072),
    photo_type TEXT CHECK(photo_type IN ('image/jpeg','image/png')),
    CHECK(num_nonnulls(photo_bytes,photo_type) IN (0,2)),
    active BOOLEAN NOT NULL DEFAULT false,
    notification_id BIGINT REFERENCES notifications(id) ON DELETE SET NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    state TEXT NOT NULL DEFAULT 'queued' CHECK(state IN ('queued','running','done','failed')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt TIMESTAMPTZ NOT NULL DEFAULT now(),
    lease BIGINT,
    lease_until TIMESTAMPTZ,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    verified_at TIMESTAMPTZ,
    UNIQUE(source,target)
);
CREATE INDEX webmentions_pending ON webmentions(next_attempt,id) WHERE state IN ('queued','running');
CREATE INDEX webmentions_owner_archive ON webmentions(owner_id,id DESC) WHERE active;
CREATE INDEX webmentions_target ON webmentions(target,id DESC) WHERE active;
CREATE TABLE webmention_status_jobs (
    status_id BIGINT PRIMARY KEY,
    revision BIGINT NOT NULL DEFAULT 1,
    next_attempt TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE webmention_deliveries (
    status_id BIGINT NOT NULL,
    target TEXT NOT NULL CHECK(length(target)<=2048),
    endpoint TEXT CHECK(length(endpoint)<=2048),
    attempts INTEGER NOT NULL DEFAULT 0,
    revision BIGINT NOT NULL DEFAULT 1,
    pending BOOLEAN NOT NULL DEFAULT true,
    next_attempt TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(status_id,target)
);
CREATE INDEX webmention_delivery_pending ON webmention_deliveries(next_attempt,status_id) WHERE pending;
-- Enqueue in the same transaction as the source change; no network on local writes.
CREATE FUNCTION queue_status_webmentions() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE item statuses; BEGIN
    IF TG_OP='DELETE' THEN item=OLD; ELSE item=NEW; END IF;
    IF item.uri IS NULL AND item.reblog_of_id IS NULL AND
       ((item.visibility IN ('public','unlisted') AND strpos(item.content,'href=')>0) OR EXISTS(SELECT 1 FROM webmention_deliveries WHERE status_id=item.id)) THEN
        INSERT INTO webmention_status_jobs(status_id) VALUES(item.id)
        ON CONFLICT(status_id) DO UPDATE SET revision=webmention_status_jobs.revision+1,next_attempt=now();
    END IF;
    IF TG_OP='DELETE' THEN RETURN OLD; ELSE RETURN NEW; END IF;
END; $$;
CREATE TRIGGER queue_status_webmentions AFTER INSERT OR DELETE OR UPDATE OF content,visibility,deleted_at ON statuses FOR EACH ROW EXECUTE FUNCTION queue_status_webmentions();
-- Explicit native public Notes about a website. The website is never an AP actor.
CREATE TABLE webmention_interactions (
    status_id BIGINT PRIMARY KEY REFERENCES statuses(id) ON DELETE CASCADE,
    source TEXT NOT NULL CHECK(length(source)<=2048),
    kind TEXT NOT NULL CHECK(kind IN ('reply','like','repost','quote'))
);
