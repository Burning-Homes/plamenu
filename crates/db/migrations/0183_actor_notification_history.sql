-- Recover unreachable managed-actor inboxes for their current owner. Keep IDs
-- and timestamps; UPDATE does not enqueue fresh streaming/push deliveries.
-- Historical editor eligibility is unknowable, so do not backfill editors.
DO $$
DECLARE old RECORD; winner BIGINT; event TEXT; scoped TEXT;
BEGIN
    FOR old IN
        SELECT n.*,r.account_id AS owner FROM notifications n
        JOIN LATERAL managed_actor_roles(n.account_id) r ON r.role='owner'
        JOIN users u ON u.account_id=r.account_id AND u.approved AND u.confirmed_at IS NOT NULL AND NOT u.disabled
        WHERE n.kind IN ('follow','follow_request','favourite','reblog','pleroma:emoji_reaction','mention','quote','status')
        ORDER BY n.id DESC
    LOOP
        event := CASE old.kind WHEN 'pleroma:emoji_reaction' THEN 'emoji_reaction' ELSE old.kind END;
        scoped := 'plamenu:actor.'||event;
        winner := NULL;
        IF old.status_id IS NOT NULL AND old.kind IN ('mention','quote','status') THEN
            SELECT n.id INTO winner FROM notifications n
            WHERE n.account_id=old.owner AND n.from_account_id=old.from_account_id
              AND n.status_id=old.status_id AND n.filtered=old.filtered
              AND n.kind IN ('mention','quote','status','plamenu:actor.comment','plamenu:actor.mention','plamenu:actor.quote','plamenu:actor.status')
            ORDER BY n.id DESC LIMIT 1;
        END IF;
        IF winner IS NULL THEN
            winner := old.id;
            UPDATE notifications SET account_id=old.owner,kind=scoped,reasons=ARRAY[scoped],group_key=NULL WHERE id=old.id;
        ELSE
            UPDATE notifications SET reasons=ARRAY(SELECT DISTINCT unnest(reasons||ARRAY[scoped])),
                kind=notification_kind_for(reasons||ARRAY[scoped],NULL) WHERE id=winner;
        END IF;
        INSERT INTO notification_actor_contexts(notification_id,actor_account_id,source_account_id,event)
            VALUES(winner,old.account_id,old.account_id,event) ON CONFLICT DO NOTHING;
        IF winner<>old.id THEN DELETE FROM notifications WHERE id=old.id; END IF;
    END LOOP;
END $$;

-- An action link never survives loss of authority, a resolved request, or
-- deactivation of either endpoint. Context itself grants no content access.
CREATE OR REPLACE FUNCTION notification_visible(recipient BIGINT, notification_id BIGINT, report_id BIGINT)
RETURNS BOOLEAN LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT notification_report_visible(recipient, report_id)
      AND NOT EXISTS (
        SELECT 1 FROM notification_actor_contexts c
        JOIN notifications n ON n.id=c.notification_id
        WHERE c.notification_id=$2 AND (
          (n.status_id IS NOT NULL AND NOT EXISTS (
            SELECT 1 FROM statuses s WHERE s.id=n.status_id AND s.deleted_at IS NULL
              AND NOT account_hidden(recipient,s.account_id)
              AND (s.account_id=recipient OR s.visibility IN ('public','unlisted','local')
                OR EXISTS(SELECT 1 FROM status_mentions m WHERE m.status_id=s.id AND m.account_id=recipient)
                OR (s.visibility='private' AND EXISTS(SELECT 1 FROM follows f WHERE f.account_id=recipient AND f.target_account_id=s.account_id AND NOT f.pending)))
          )) OR
          (c.event IN ('follow_request','invitation','transfer') AND (
            NOT EXISTS(SELECT 1 FROM accounts a WHERE a.id=recipient AND a.deleted_at IS NULL AND a.suspended_at IS NULL)
            OR NOT EXISTS(SELECT 1 FROM managed_actor_roles(c.actor_account_id) r WHERE r.role='owner')
            OR NOT CASE c.event
              WHEN 'invitation' THEN EXISTS(SELECT 1 FROM video_channel_grants g WHERE g.channel_id=c.actor_account_id AND g.account_id=recipient AND NOT g.accepted)
              WHEN 'transfer' THEN EXISTS(SELECT 1 FROM video_channels v WHERE v.account_id=c.actor_account_id AND v.transfer_to=recipient)
              ELSE EXISTS(SELECT 1 FROM managed_actor_roles(c.actor_account_id) r WHERE r.account_id=recipient AND r.role IN ('owner','editor','moderator'))
                AND EXISTS(SELECT 1 FROM follows f WHERE f.account_id=n.from_account_id AND f.target_account_id=c.actor_account_id AND f.pending)
            END
          ))
        )
      )
$$;
