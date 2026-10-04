-- Fan-outs enqueue many jobs at the same run_at. The claim's (run_at, id)
-- order needs both columns: a run_at-only index still sorts the tied backlog
-- before FOR UPDATE SKIP LOCKED can take its bounded batch. Keep the existing
-- due-time index; no queue data or lease/cancellation policy changes here.
-- This adds index maintenance on enqueue/retry/lease in exchange for avoiding
-- a full tied-timestamp scan and sort on each claim.
CREATE INDEX idx_delivery_jobs_due_order ON delivery_jobs (run_at, id);
