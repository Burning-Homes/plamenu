-- Native per-kind channel selection; push subscription choices remain separate.
CREATE TABLE notification_delivery_preferences (
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK(length(kind) BETWEEN 1 AND 80),
    in_app BOOLEAN NOT NULL DEFAULT true,
    email BOOLEAN NOT NULL DEFAULT false,
    email_digest BOOLEAN NOT NULL DEFAULT false,
    email_since TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(account_id,kind)
);
CREATE TABLE notification_mail_batches (
    email_job_id BIGINT PRIMARY KEY REFERENCES email_jobs(id) ON DELETE CASCADE,
    account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    channel TEXT NOT NULL CHECK(channel IN ('email','email_digest')),
    UNIQUE(account_id,channel)
);
CREATE TABLE notification_mail_receipts (
    notification_id BIGINT NOT NULL REFERENCES notifications(id) ON DELETE CASCADE,
    channel TEXT NOT NULL CHECK(channel IN ('email','email_digest')),
    email_job_id BIGINT REFERENCES email_jobs(id) ON DELETE SET NULL,
    sent_at TIMESTAMPTZ,
    retry_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY(notification_id,channel)
);
CREATE INDEX notification_mail_receipts_job ON notification_mail_receipts(email_job_id)
    WHERE email_job_id IS NOT NULL;
CREATE TABLE notification_digest_clock (
    account_id BIGINT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    next_digest_at TIMESTAMPTZ NOT NULL
);
-- Account erasure cancels the queued envelope as well as its owned metadata.
CREATE FUNCTION notification_mail_batch_removed() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM email_jobs WHERE id=OLD.email_job_id;
    RETURN NULL;
END $$;
CREATE TRIGGER notification_mail_batch_removed AFTER DELETE ON notification_mail_batches
    FOR EACH ROW EXECUTE FUNCTION notification_mail_batch_removed();
CREATE INDEX notifications_retention_age ON notifications(created_at,id);
ALTER TABLE webmentions ADD COLUMN announced BOOLEAN NOT NULL DEFAULT false;
UPDATE webmentions SET announced=true WHERE active OR entry IS NOT NULL;
