-- A notification is a pointer to a moderation case, never a permanent grant
-- to its contents. Keep listings, counters and queued delivery on the same
-- current authority decision, including after a moderator is removed.
CREATE FUNCTION notification_report_visible(recipient BIGINT, report_id BIGINT)
RETURNS BOOLEAN LANGUAGE sql STABLE PARALLEL SAFE AS $$
    SELECT report_id IS NULL OR EXISTS (
        SELECT 1 FROM reports r
        JOIN accounts viewer ON viewer.id = recipient
        WHERE r.id = report_id
          AND viewer.deleted_at IS NULL AND viewer.suspended_at IS NULL
          AND (
              EXISTS (
                  SELECT 1 FROM users u JOIN user_roles role ON role.id = u.role_id
                  WHERE u.account_id = recipient AND NOT u.disabled
                    -- ADMINISTRATOR | MANAGE_REPORTS (role::permission).
                    AND role.permissions & 17 <> 0
              ) OR EXISTS (
                  SELECT 1 FROM group_affiliations affiliation
                  JOIN groups g ON g.account_id = affiliation.group_account_id
                  JOIN accounts community ON community.id = g.account_id
                  WHERE g.account_id = r.group_account_id
                    AND affiliation.account_id = recipient
                    AND affiliation.affiliation IN ('owner', 'moderator')
                    AND (affiliation.expires_at IS NULL OR affiliation.expires_at > now())
                    AND community.deleted_at IS NULL AND community.suspended_at IS NULL
              )
          )
    )
$$;
