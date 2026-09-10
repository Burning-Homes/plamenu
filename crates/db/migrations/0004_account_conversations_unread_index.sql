-- The web client's unread-conversations nav probe runs on every page load;
-- partial, so accounts with everything read (the common case) answer from a
-- near-empty index instead of walking all their conversation rows.
CREATE INDEX account_conversations_unread_idx
    ON account_conversations (account_id)
    WHERE unread;
