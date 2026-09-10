-- The admin audit log advertises itself as append-only, yet
-- `admin_action_logs.account_id` referenced the acting moderator with
-- `ON DELETE CASCADE`: hard-deleting a moderator or admin silently erased every
-- action they had ever recorded. Snapshot the actor's handle at write time (the
-- way `human_identifier` already snapshots the *target*) and stop cascading the
-- history away — a deleted actor now nulls the reference and the row survives.
-- QC audit finding #49.

-- Snapshot column; DEFAULT '' mirrors `human_identifier` and lets the ALTER
-- succeed against existing rows before the backfill fills them in.
ALTER TABLE admin_action_logs
    ADD COLUMN actor_username text NOT NULL DEFAULT '';

-- Backfill the snapshot for existing rows from the still-present acting account.
UPDATE admin_action_logs l
   SET actor_username = a.username
  FROM accounts a
 WHERE a.id = l.account_id;

-- The acting account may now vanish without taking its history with it.
ALTER TABLE admin_action_logs
    ALTER COLUMN account_id DROP NOT NULL;

ALTER TABLE admin_action_logs
    DROP CONSTRAINT admin_action_logs_account_id_fkey,
    ADD  CONSTRAINT admin_action_logs_account_id_fkey
        FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE SET NULL;
