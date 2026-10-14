//! Explicit authority for hosted channels and communities.
use crate::{DbError, PgPool};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Role {
    pub account_id: i64,
    pub role: String,
}

pub async fn kind(pool: &PgPool, actor: i64) -> Result<Option<String>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT CASE WHEN EXISTS(SELECT 1 FROM video_channels WHERE account_id=$1) THEN 'channel'
                     WHEN EXISTS(SELECT 1 FROM groups WHERE account_id=$1) THEN 'group' END",
    )
    .bind(actor)
    .fetch_one(pool)
    .await?)
}

pub async fn roles(pool: &PgPool, actor: i64) -> Result<Vec<Role>, DbError> {
    Ok(sqlx::query_as::<_, Role>(
        "SELECT account_id,role FROM managed_actor_roles($1)
         ORDER BY CASE role WHEN 'owner' THEN 0 WHEN 'editor' THEN 1 WHEN 'moderator' THEN 1 ELSE 2 END,account_id",
    ).bind(actor).fetch_all(pool).await?)
}

/// Routine channel activity reaches the owner and accepted editors. A
/// publisher's ordinary personal notifications remain independent of this role.
pub async fn notification_recipients(pool: &PgPool, actor: i64) -> Result<Vec<i64>, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT r.account_id FROM managed_actor_roles($1) r
         JOIN users u ON u.account_id=r.account_id
         WHERE r.role IN ('owner','editor','moderator')
           AND u.approved AND u.confirmed_at IS NOT NULL AND NOT u.disabled
         ORDER BY r.account_id",
    )
    .bind(actor)
    .fetch_all(pool)
    .await?)
}

/// Resolve a whole mention audience without adding a query per recipient.
pub async fn notification_recipient_map(
    pool: &PgPool,
    actors: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<i64>>, DbError> {
    if actors.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT a.id,recipients.account_id FROM accounts a
         LEFT JOIN LATERAL (
            SELECT DISTINCT r.account_id FROM managed_actor_roles(a.id) r
            JOIN users u ON u.account_id=r.account_id
            WHERE r.role IN ('owner','editor','moderator') AND u.approved
              AND u.confirmed_at IS NOT NULL AND NOT u.disabled
         ) recipients ON true
         WHERE a.id=ANY($1) AND (EXISTS(SELECT 1 FROM video_channels c WHERE c.account_id=a.id)
             OR EXISTS(SELECT 1 FROM groups g WHERE g.account_id=a.id))",
    )
    .bind(actors)
    .fetch_all(pool)
    .await?;
    let mut result: std::collections::HashMap<i64, Vec<i64>> = std::collections::HashMap::new();
    for (actor, recipient) in rows {
        let ids = result.entry(actor).or_default();
        if let Some(recipient) = recipient {
            ids.push(recipient);
        }
    }
    Ok(result)
}

/// Public reverse links expose ownership/moderation, never following lists or
/// pending invitations. Accepted collaborators are listed on their channel.
pub async fn connections(pool: &PgPool, person: i64) -> Result<Vec<Role>, DbError> {
    Ok(sqlx::query_as::<_, Role>(
        "SELECT a.id AS account_id,related.role FROM (
            SELECT c.account_id,'owner'::text AS role FROM video_channels c WHERE c.owner_account_id=$1
            UNION ALL
            SELECT g.group_account_id,g.affiliation FROM group_affiliations g
            JOIN groups community ON community.account_id=g.group_account_id
            WHERE g.account_id=$1 AND g.affiliation IN ('owner','moderator')
              AND (g.expires_at IS NULL OR g.expires_at>now())
         ) related JOIN accounts a ON a.id=related.account_id
         WHERE a.deleted_at IS NULL AND a.suspended_at IS NULL ORDER BY a.id",
    ).bind(person).fetch_all(pool).await?)
}
