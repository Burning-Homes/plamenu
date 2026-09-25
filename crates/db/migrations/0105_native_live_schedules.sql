-- Keep the full set of locally authored future starts alongside the legacy
-- next-start column. The latter remains a fast, compatible projection of the
-- first array element; both change together in broadcast row transactions.
ALTER TABLE native_live_broadcasts
    ADD COLUMN scheduled_starts TIMESTAMPTZ[] NOT NULL DEFAULT '{}'::timestamptz[]
    CHECK (cardinality(scheduled_starts) <= 16);

UPDATE native_live_broadcasts
SET scheduled_starts = ARRAY[scheduled_at]
WHERE scheduled_at IS NOT NULL;

ALTER TABLE native_live_broadcasts
    ADD CONSTRAINT native_live_schedule_first
    CHECK (scheduled_at IS NOT DISTINCT FROM scheduled_starts[1]);
