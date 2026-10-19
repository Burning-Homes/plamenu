-- A member can hide a catalogue card before importing its account. The pair
-- of foreign keys bounds storage by members and retained catalogue entries.
CREATE TABLE discovery_dismissals (
    account_id bigint NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    entry_id bigint NOT NULL REFERENCES discovery_entries(id) ON DELETE CASCADE,
    PRIMARY KEY (account_id, entry_id)
);
