-- Current authority shared by profiles, notification routing and action gates.
-- Pending channel invitations never grant a public role or inbox authority.
CREATE FUNCTION managed_actor_roles(actor_id BIGINT)
RETURNS TABLE(account_id BIGINT, role TEXT) LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT p.id, authority.role FROM (
        SELECT c.owner_account_id AS person, 'owner'::text AS role
        FROM video_channels c WHERE c.account_id = actor_id
        UNION ALL
        SELECT g.account_id, g.role FROM video_channel_grants g
        JOIN video_channels c ON c.account_id = g.channel_id
        WHERE g.channel_id = actor_id AND g.accepted AND g.account_id <> c.owner_account_id
        UNION ALL
        SELECT g.account_id, g.affiliation FROM group_affiliations g
        JOIN groups community ON community.account_id = g.group_account_id
        WHERE g.group_account_id = actor_id AND g.affiliation IN ('owner', 'moderator')
          AND (g.expires_at IS NULL OR g.expires_at > now())
    ) authority
    JOIN accounts p ON p.id = authority.person
    JOIN accounts managed ON managed.id = actor_id
    WHERE p.deleted_at IS NULL AND p.suspended_at IS NULL
      AND managed.deleted_at IS NULL AND managed.suspended_at IS NULL
      AND NOT EXISTS (
          SELECT 1 FROM video_channels c JOIN accounts owner ON owner.id = c.owner_account_id
          WHERE c.account_id = actor_id AND (owner.deleted_at IS NOT NULL OR owner.suspended_at IS NOT NULL)
      )
$$;

CREATE TABLE notification_actor_contexts (
    notification_id BIGINT NOT NULL REFERENCES notifications(id) ON DELETE CASCADE,
    actor_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    source_account_id BIGINT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    event TEXT NOT NULL CHECK (event IN ('follow','follow_request','favourite','reblog','emoji_reaction','comment','mention','quote','status','invitation','transfer','report')),
    PRIMARY KEY(notification_id, actor_account_id, event)
);
CREATE INDEX notification_actor_contexts_source ON notification_actor_contexts(source_account_id, event);
CREATE INDEX notification_actor_contexts_actor ON notification_actor_contexts(actor_account_id, event);

-- Undo/withdrawal addresses the original actor or post author, rather than
-- accidentally clearing a controller's unrelated personal interaction.
CREATE FUNCTION notification_source_ids(source BIGINT, senders BIGINT[])
RETURNS TABLE(notification_id BIGINT) LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT n.id FROM notifications n
    WHERE n.account_id=source AND n.from_account_id=ANY(senders)
      AND NOT EXISTS(SELECT 1 FROM notification_actor_contexts c WHERE c.notification_id=n.id)
    UNION
    SELECT c.notification_id FROM notification_actor_contexts c
    JOIN notifications n ON n.id=c.notification_id
    WHERE c.source_account_id=source AND n.from_account_id=ANY(senders)
$$;

CREATE FUNCTION notification_base_kind(kind TEXT) RETURNS TEXT
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE kind
        WHEN 'plamenu:actor.comment' THEN 'mention'
        WHEN 'plamenu:actor.mention' THEN 'mention'
        WHEN 'plamenu:actor.quote' THEN 'quote'
        WHEN 'plamenu:actor.status' THEN 'status'
        WHEN 'plamenu:actor.follow' THEN 'follow'
        WHEN 'plamenu:actor.follow_request' THEN 'follow_request'
        WHEN 'plamenu:actor.favourite' THEN 'favourite'
        WHEN 'plamenu:actor.reblog' THEN 'reblog'
        WHEN 'plamenu:actor.emoji_reaction' THEN 'pleroma:emoji_reaction'
        WHEN 'plamenu:actor.invitation' THEN 'follow_request'
        WHEN 'plamenu:actor.transfer' THEN 'follow_request'
        ELSE kind END
$$;

CREATE FUNCTION notification_reason_priority(kind TEXT) RETURNS INTEGER
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE kind
        WHEN 'plamenu:actor.comment' THEN 0 WHEN 'plamenu:actor.mention' THEN 1
        WHEN 'plamenu:actor.quote' THEN 2 WHEN 'plamenu:actor.status' THEN 3
        WHEN 'mention' THEN 4 WHEN 'quote' THEN 5 WHEN 'status' THEN 6 ELSE 7 END
$$;

-- Channel/group post reasons share the existing canonical post notification.
DROP INDEX idx_notifications_one_post_reason_surface;
CREATE UNIQUE INDEX idx_notifications_one_post_reason_surface
    ON notifications(account_id, from_account_id, status_id, filtered)
    WHERE status_id IS NOT NULL AND kind IN (
        'mention','quote','status',
        'plamenu:actor.comment','plamenu:actor.mention','plamenu:actor.quote','plamenu:actor.status'
    );

CREATE OR REPLACE FUNCTION notification_kind_for(reasons TEXT[], allowed TEXT[])
RETURNS TEXT LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT reason FROM unnest(reasons) WITH ORDINALITY AS r(reason, ordinal)
    WHERE allowed IS NULL OR reason = ANY(allowed)
    ORDER BY notification_reason_priority(reason), ordinal
    LIMIT 1
$$;

-- Sensitive work items require current authority, including when queued for
-- delivery before a role was revoked. Public historical activity can remain
-- in its recipient's history; it conveys no authority to perform an action.
CREATE FUNCTION notification_visible(recipient BIGINT, notification_id BIGINT, report_id BIGINT)
RETURNS BOOLEAN LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT notification_report_visible(recipient, report_id) AND NOT EXISTS (
        SELECT 1 FROM notification_actor_contexts c
        WHERE c.notification_id = $2
          AND c.event IN ('follow_request','invitation','transfer')
          AND NOT CASE c.event
              WHEN 'invitation' THEN EXISTS (
                  SELECT 1 FROM video_channel_grants g
                  WHERE g.channel_id = c.actor_account_id AND g.account_id = recipient AND NOT g.accepted
              )
              WHEN 'transfer' THEN EXISTS (
                  SELECT 1 FROM video_channels channel
                  WHERE channel.account_id = c.actor_account_id AND channel.transfer_to = recipient
              )
              ELSE EXISTS (
                  SELECT 1 FROM managed_actor_roles(c.actor_account_id) r
                  WHERE r.account_id = recipient AND r.role IN ('owner','editor','moderator')
              )
          END
    )
$$;

-- Existing clients can keep their standard push switches. An explicit switch
-- for the extension overrides that fallback, including an explicit false.
CREATE OR REPLACE FUNCTION web_push_fanout() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.filtered THEN RETURN NULL; END IF;
    INSERT INTO push_delivery_jobs(subscription_id,notification_id)
    SELECT s.id,NEW.id FROM web_push_subscriptions s JOIN users u ON u.id=s.user_id
    WHERE u.account_id=NEW.account_id
      AND EXISTS (
          SELECT 1 FROM unnest(NEW.reasons) reason
          WHERE COALESCE(
              (SELECT a.enabled FROM web_push_alerts a WHERE a.subscription_id=s.id AND a.kind=reason),
              (SELECT a.enabled FROM web_push_alerts a WHERE a.subscription_id=s.id AND a.kind=notification_base_kind(reason)),false)
      )
      AND CASE s.policy
          WHEN 'all' THEN true
          WHEN 'followed' THEN EXISTS(SELECT 1 FROM follows f WHERE f.account_id=NEW.account_id AND f.target_account_id=NEW.from_account_id AND NOT f.pending)
          WHEN 'follower' THEN EXISTS(SELECT 1 FROM follows f WHERE f.account_id=NEW.from_account_id AND f.target_account_id=NEW.account_id AND NOT f.pending)
          ELSE false END;
    RETURN NULL;
END $$;
