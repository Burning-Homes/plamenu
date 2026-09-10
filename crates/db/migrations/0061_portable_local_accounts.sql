-- A gateway actor is remotely owned but has a real account in this
-- instance's handle namespace.  Keep that fact separate from `domain IS
-- NULL`, which means that Plamenu owns and signs the ActivityPub identity.
ALTER TABLE accounts
    ADD COLUMN portable boolean NOT NULL DEFAULT false;

UPDATE accounts
SET portable = true,
    is_internal = false
WHERE id IN (SELECT account_id FROM gateway_actors);

-- Local users/groups and portable users share one case-insensitive handle
-- namespace.  Internal implementation actors remain outside it.
CREATE UNIQUE INDEX accounts_local_handle
    ON accounts (lower(username))
    WHERE (domain IS NULL AND NOT is_internal) OR portable;

-- The same remote activity can legitimately be delivered to several
-- portable inboxes on one gateway.  Deduplicate within a collection, while
-- the outbox handler separately enforces that a client never reuses one of
-- its portable IDs.
ALTER TABLE gateway_collection_items
    DROP CONSTRAINT gateway_collection_items_object_uri_key;
ALTER TABLE gateway_collection_items
    ADD CONSTRAINT gateway_collection_items_owner_collection_object
    UNIQUE (account_id, collection, object_uri);
CREATE INDEX gateway_collection_items_object_uri
    ON gateway_collection_items (object_uri);

-- Portable actors carry the gateway's domain only as their federated acct
-- domain.  Instance and user domain blocks must not turn that into a remote
-- moderation boundary: inside this server they are ordinary local-account
-- participants, with their own account-level moderation state.
CREATE OR REPLACE FUNCTION public.account_silenced(author bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (
        SELECT 1 FROM accounts a
        WHERE a.id = author
          AND (
            a.silenced_at IS NOT NULL
            OR (NOT a.portable AND a.domain IS NOT NULL AND EXISTS (
                SELECT 1 FROM domain_blocks db
                WHERE db.domain = a.domain AND db.severity = 'silence'))
          )
    )
$$;

CREATE OR REPLACE FUNCTION public.account_relationship_hidden(viewer bigint, author bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT viewer IS NOT NULL AND viewer <> author AND (
        EXISTS (SELECT 1 FROM blocks b
                WHERE (b.account_id = viewer AND b.target_account_id = author)
                   OR (b.account_id = author AND b.target_account_id = viewer))
        OR EXISTS (SELECT 1 FROM mutes m
                   WHERE m.account_id = viewer AND m.target_account_id = author
                     AND (m.expires_at IS NULL OR m.expires_at > now()))
        OR EXISTS (SELECT 1
                   FROM accounts a
                   JOIN account_domain_blocks adb
                     ON adb.account_id = viewer AND adb.domain = a.domain
                   WHERE a.id = author AND NOT a.portable)
        OR EXISTS (SELECT 1
                   FROM accounts v
                   JOIN account_domain_blocks adb
                     ON adb.account_id = author AND adb.domain = v.domain
                   WHERE v.id = viewer AND NOT v.portable)
    )
$$;

CREATE OR REPLACE FUNCTION public.sender_filtered(recipient bigint, sender bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (SELECT 1 FROM accounts a
                   WHERE a.id = sender AND a.suspended_at IS NOT NULL)
        OR EXISTS (SELECT 1 FROM blocks b
                   WHERE b.account_id = recipient AND b.target_account_id = sender)
        OR EXISTS (SELECT 1 FROM mutes m
                   WHERE m.account_id = recipient AND m.target_account_id = sender
                     AND m.hide_notifications
                     AND (m.expires_at IS NULL OR m.expires_at > now()))
        OR EXISTS (SELECT 1
                   FROM accounts a
                   JOIN account_domain_blocks adb
                     ON adb.account_id = recipient AND adb.domain = a.domain
                   WHERE a.id = sender AND NOT a.portable)
$$;
