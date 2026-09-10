-- Keep the full availability predicate authoritative for callers that do not
-- already join accounts, while exposing the pre-0057 relationship-only part
-- to hot queries which have already checked suspended_at explicitly.
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
                   WHERE a.id = author)
        OR EXISTS (SELECT 1
                   FROM accounts v
                   JOIN account_domain_blocks adb
                     ON adb.account_id = author AND adb.domain = v.domain
                   WHERE v.id = viewer)
    )
$$;

CREATE INDEX idx_accounts_suspended_id
    ON accounts (id) WHERE suspended_at IS NOT NULL;

CREATE INDEX idx_follows_target_accepted_page
    ON follows (target_account_id, id DESC) INCLUDE (account_id)
    WHERE NOT pending;

CREATE OR REPLACE FUNCTION public.account_hidden(viewer bigint, author bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (SELECT 1 FROM accounts a
                   WHERE a.id = author AND a.suspended_at IS NOT NULL)
        OR account_relationship_hidden(viewer, author)
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
                   WHERE a.id = sender)
$$;
