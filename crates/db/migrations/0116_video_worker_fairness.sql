-- Persist uploader turns across worker/process restarts. FIFO remains within
-- an uploader, but one account's bulk upload cannot monopolize free slots.
CREATE TABLE video_worker_turns (
    account_id bigint PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
    last_claimed_at timestamptz NOT NULL
);
