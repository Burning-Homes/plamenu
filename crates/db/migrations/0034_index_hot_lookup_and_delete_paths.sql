-- Index the two lookups that had none: the signature key id every signed
-- request resolves, and the referencing side of the foreign keys a delete
-- walks (`BENCH_AUDIT_PLAN.md` C1 and C2).
--
-- Both were found by benchmarking, and neither is visible in a latency budget:
-- one sits behind an inbox POST that already costs 16 ms, and the other only
-- costs anything when something is deleted, which no benchmark did.
--
-- The rule applied to the foreign keys below: index the referencing column when
-- the referencing table grows with ordinary instance activity, so its scan cost
-- grows without bound. Deliberately left unindexed are the ones on
-- moderation-log tables (`appeals`, `reports`, `report_notes`,
-- `account_warnings`, `account_moderation_notes`, `announcement_*`,
-- `email_domain_blocks`) — there a sequential scan is bounded by how much
-- moderating a human has done, not by how much traffic the instance carries.

-- `account::find_by_key_id` runs on every signed inbox POST, and on every
-- signed ActivityPub GET under `authorized_fetch`, which is the shipped
-- default. It had no index at all: against 61,306 accounts the lookup was a
-- sequential scan reading 7,767 blocks, 99 ms cold and 16 ms warm, and it was
-- essentially the whole of what the inbox-ingest benchmarks were measuring.
-- Partial because a local account has no remote key id to resolve, and the
-- probe never asks for NULL.
CREATE INDEX idx_accounts_public_key_id
    ON accounts (public_key_id)
    WHERE public_key_id IS NOT NULL;

-- Deleting a status. `statuses_cleanup` runs this ~50 times a minute and the
-- daily maintenance sweep ~2,000 times, and each one made Postgres scan every
-- table below from end to end to find the rows to cascade or null out —
-- `conversations` alone is 339,580 rows and 18 MB, scanned once per deleted
-- status while the deleting transaction holds its locks.
CREATE INDEX idx_conversations_root_status_id
    ON conversations (root_status_id)
    WHERE root_status_id IS NOT NULL;
CREATE INDEX idx_link_crawl_jobs_status_id ON link_crawl_jobs (status_id);
CREATE INDEX idx_custom_filter_statuses_status_id ON custom_filter_statuses (status_id);

-- Deleting an account — the mass-deletion storm every server eventually meets,
-- where one `DELETE FROM accounts` walks all of these inside a single
-- transaction. `tag_usages` is 694,987 rows / 39 MB and `preview_card_usages`
-- 99,764; both cascade, so the scan is per deleted account.
CREATE INDEX idx_tag_usages_account_id ON tag_usages (account_id);
CREATE INDEX idx_preview_card_usages_account_id ON preview_card_usages (account_id);
CREATE INDEX idx_conversations_owner_account_id
    ON conversations (owner_account_id)
    WHERE owner_account_id IS NOT NULL;
CREATE INDEX idx_preview_cards_author_account_id
    ON preview_cards (author_account_id)
    WHERE author_account_id IS NOT NULL;
CREATE INDEX idx_status_tombstones_account_id ON status_tombstones (account_id);
CREATE INDEX idx_account_conversations_conversation_id
    ON account_conversations (conversation_id);
