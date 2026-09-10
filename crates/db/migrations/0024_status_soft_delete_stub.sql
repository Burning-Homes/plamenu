-- Soft-delete "stub" support (GoToSocial-style tombstone-for-threading).
--
-- When a status that still has replies is deleted, we keep the row as a
-- content-nulled placeholder (`deleted_at` set) so the reply *tree* does not
-- lose its descendant subtree — the self-FK `statuses_in_reply_to_id_fkey ON
-- DELETE SET NULL` would otherwise detach the children. A leaf stub (nothing
-- replies to it any more) is hard-wiped later by the maintenance GC.
--
-- `deleted_at` marks a stub. The partial index covers only stubs (a tiny
-- minority of rows), so the GC leaf-sweep is cheap and the common all-NULL
-- case adds no write amplification to the live set.
ALTER TABLE statuses ADD COLUMN deleted_at timestamptz;

CREATE INDEX statuses_deleted_at_idx ON statuses (deleted_at) WHERE deleted_at IS NOT NULL;
