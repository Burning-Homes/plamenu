-- Suspension is an account-wide availability state, not merely an admin
-- label.  Keep these two shared predicates authoritative so every timeline,
-- collection and notification query that already uses them gains the same
-- semantics.

CREATE OR REPLACE FUNCTION public.account_hidden(viewer bigint, author bigint) RETURNS boolean
    LANGUAGE sql STABLE
    AS $$
    SELECT EXISTS (SELECT 1 FROM accounts a
                   WHERE a.id = author AND a.suspended_at IS NOT NULL)
        OR (viewer IS NOT NULL AND viewer <> author AND (
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
        ))
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

-- A reversible suspension has Mastodon's 30-day grace period. Lifting the
-- suspension removes this row; once due, maintenance purges the user data and
-- leaves the reserved account tombstone.
CREATE TABLE public.account_deletion_requests (
    account_id bigint PRIMARY KEY REFERENCES public.accounts(id) ON DELETE CASCADE,
    due_at timestamp with time zone NOT NULL DEFAULT (now() + interval '30 days'),
    created_at timestamp with time zone NOT NULL DEFAULT now()
);

CREATE INDEX account_deletion_requests_due_idx
    ON public.account_deletion_requests (due_at);
