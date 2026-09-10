-- A profile view is an automatic hydration hint, not an instruction to retry
-- on every GET.  Keep its next eligible instant alongside the actor state so
-- terminal outcomes cannot be turned straight back into queued work by a
-- reload, another process, or a third-party client polling the timeline.
ALTER TABLE remote_history_states
    ADD COLUMN automatic_retry_at timestamptz;

-- Existing terminal failures predate the admission distinction.  Give them a
-- conservative initial quiet period on upgrade instead of immediately
-- replaying every failed actor as profiles are opened.
UPDATE remote_history_states
SET automatic_retry_at = CASE
        WHEN state = 'unsupported' THEN updated_at + interval '7 days'
        WHEN state IN ('partial', 'backoff') AND last_error_class IS NOT NULL
            THEN updated_at + interval '6 hours'
        ELSE automatic_retry_at
    END
WHERE state = 'unsupported'
   OR (state IN ('partial', 'backoff') AND last_error_class IS NOT NULL);

CREATE INDEX idx_remote_history_states_automatic_retry
    ON remote_history_states (automatic_retry_at)
    WHERE automatic_retry_at IS NOT NULL;
