-- A post can reach one recipient for several simultaneous reasons: they asked
-- to be notified when its author posts, the post quotes them, and/or it
-- mentions them. Keep the complete reason set while presenting one canonical
-- notification (`mention` > `quote` > `status`).
ALTER TABLE notifications
    ADD COLUMN reasons TEXT[];

UPDATE notifications SET reasons = ARRAY[kind];

ALTER TABLE notifications
    ALTER COLUMN reasons SET DEFAULT ARRAY[]::TEXT[],
    ALTER COLUMN reasons SET NOT NULL,
    ADD CONSTRAINT notifications_reasons_not_empty CHECK (cardinality(reasons) > 0),
    ADD CONSTRAINT notifications_kind_is_reason CHECK (kind = ANY(reasons));

-- Repair pre-existing overlap before enforcing the invariant. Retain the
-- newest id so notification markers and chronological placement never move
-- backwards, and keep policy-filtered and ordinary surfaces independent.
WITH overlap AS (
    SELECT account_id, from_account_id, status_id, filtered,
           max(id) AS winner_id,
           CASE
               WHEN bool_or(kind = 'mention') THEN 'mention'
               WHEN bool_or(kind = 'quote') THEN 'quote'
               ELSE 'status'
           END AS winner_kind,
           ARRAY_REMOVE(ARRAY[
               CASE WHEN bool_or(kind = 'mention') THEN 'mention' END,
               CASE WHEN bool_or(kind = 'quote') THEN 'quote' END,
               CASE WHEN bool_or(kind = 'status') THEN 'status' END
           ], NULL) AS reasons
    FROM notifications
    WHERE status_id IS NOT NULL
      AND kind IN ('mention', 'quote', 'status')
    GROUP BY account_id, from_account_id, status_id, filtered
    HAVING count(*) > 1
)
UPDATE notifications n SET kind = o.winner_kind, reasons = o.reasons
FROM overlap o
WHERE n.id = o.winner_id;

WITH winners AS (
    SELECT account_id, from_account_id, status_id, filtered, max(id) AS winner_id
    FROM notifications
    WHERE status_id IS NOT NULL
      AND kind IN ('mention', 'quote', 'status')
    GROUP BY account_id, from_account_id, status_id, filtered
)
DELETE FROM notifications n USING winners w
WHERE n.account_id = w.account_id
  AND n.from_account_id = w.from_account_id
  AND n.status_id = w.status_id
  AND n.filtered = w.filtered
  AND n.kind IN ('mention', 'quote', 'status')
  AND n.id <> w.winner_id;

CREATE UNIQUE INDEX idx_notifications_one_post_reason_surface
    ON notifications (account_id, from_account_id, status_id, filtered)
    WHERE status_id IS NOT NULL
      AND kind IN ('mention', 'quote', 'status');

-- Select the highest-priority reason allowed by a listing's type filter. This
-- preserves `types[]`/`exclude_types[]`: asking only for status notifications
-- still finds a coalesced mention+status row and presents its status reason.
CREATE FUNCTION notification_kind_for(reasons TEXT[], allowed TEXT[])
RETURNS TEXT
LANGUAGE sql
IMMUTABLE
PARALLEL SAFE
AS $$
    SELECT CASE
        WHEN 'mention' = ANY(reasons)
         AND (allowed IS NULL OR 'mention' = ANY(allowed)) THEN 'mention'
        WHEN 'quote' = ANY(reasons)
         AND (allowed IS NULL OR 'quote' = ANY(allowed)) THEN 'quote'
        WHEN 'status' = ANY(reasons)
         AND (allowed IS NULL OR 'status' = ANY(allowed)) THEN 'status'
        ELSE (
            SELECT reason
            FROM unnest(reasons) AS reason
            WHERE allowed IS NULL OR reason = ANY(allowed)
            LIMIT 1
        )
    END
$$;

-- A canonical notification produces one push job when any of its contributing
-- reasons is enabled. Delivery chooses the highest-priority enabled reason.
CREATE OR REPLACE FUNCTION web_push_fanout() RETURNS trigger
    LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.filtered THEN
        RETURN NULL;
    END IF;
    INSERT INTO push_delivery_jobs (subscription_id, notification_id)
    SELECT DISTINCT s.id, NEW.id
    FROM web_push_subscriptions s
    JOIN users u ON u.id = s.user_id
    JOIN web_push_alerts a
      ON a.subscription_id = s.id AND a.kind = ANY(NEW.reasons) AND a.enabled
    WHERE u.account_id = NEW.account_id
      AND CASE s.policy
            WHEN 'all' THEN true
            WHEN 'followed' THEN EXISTS (
                SELECT 1 FROM follows f
                WHERE f.account_id = NEW.account_id
                  AND f.target_account_id = NEW.from_account_id
                  AND NOT f.pending)
            WHEN 'follower' THEN EXISTS (
                SELECT 1 FROM follows f
                WHERE f.account_id = NEW.from_account_id
                  AND f.target_account_id = NEW.account_id
                  AND NOT f.pending)
            ELSE false
          END;
    RETURN NULL;
END $$;
