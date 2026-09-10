//! Follow suggestions — Mastodon's `AccountSuggestions`. Three SQL-computed
//! sources (friends-of-friends, globally most-followed, most-interacted-with),
//! each excluding the viewer, accounts they already follow, hidden
//! (blocked/muted/domain-blocked) accounts, dismissed suggestions, and
//! non-discoverable/suspended/silenced accounts. The per-account suppressions
//! table (`follow_recommendation_mutes`) backs `DELETE /suggestions/{id}`.

use sqlx::PgPool;

use crate::DbError;

/// Second-degree connections: accounts followed by accounts the viewer follows,
/// ranked by how many of the viewer's followees follow them (Mastodon's
/// `FriendsOfFriendsSource`).
pub async fn friends_of_friends(
    pool: &PgPool,
    viewer: i64,
    limit: i64,
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"
        SELECT f2.target_account_id AS "id!"
        FROM follows f1
        JOIN follows f2 ON f2.account_id = f1.target_account_id
        JOIN accounts a ON a.id = f2.target_account_id
        WHERE f1.account_id = $1
          AND f2.target_account_id <> $1
          AND a.discoverable IS TRUE
          AND a.actor_type IS DISTINCT FROM 'Group'
          AND a.suspended_at IS NULL AND a.silenced_at IS NULL
          AND NOT account_hidden($1, f2.target_account_id)
          AND NOT EXISTS (
              SELECT 1 FROM follows me
              WHERE me.account_id = $1 AND me.target_account_id = f2.target_account_id)
          AND NOT EXISTS (
              SELECT 1 FROM follow_recommendation_mutes s
              WHERE s.account_id = $1 AND s.target_account_id = f2.target_account_id)
        GROUP BY f2.target_account_id
        ORDER BY count(*) DESC, f2.target_account_id DESC
        LIMIT $2
        "#,
        viewer,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The globally most-followed eligible accounts (Mastodon's `GlobalSource`,
/// `most_followed`).
pub async fn most_followed(pool: &PgPool, viewer: i64, limit: i64) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"
        SELECT a.id AS "id!"
        FROM accounts a
        JOIN follows f ON f.target_account_id = a.id
        WHERE a.id <> $1
          AND a.discoverable IS TRUE
          AND a.actor_type IS DISTINCT FROM 'Group'
          AND a.suspended_at IS NULL AND a.silenced_at IS NULL
          AND NOT account_hidden($1, a.id)
          AND NOT EXISTS (
              SELECT 1 FROM follows me
              WHERE me.account_id = $1 AND me.target_account_id = a.id)
          AND NOT EXISTS (
              SELECT 1 FROM follow_recommendation_mutes s
              WHERE s.account_id = $1 AND s.target_account_id = a.id)
        GROUP BY a.id
        ORDER BY count(f.id) DESC, a.id DESC
        LIMIT $2
        "#,
        viewer,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// The most-interacted-with eligible accounts — those whose statuses drew the
/// most favourites and reblogs (Mastodon's `GlobalSource`, `most_interactions`).
pub async fn most_interactions(
    pool: &PgPool,
    viewer: i64,
    limit: i64,
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"
        SELECT a.id AS "id!"
        FROM accounts a
        JOIN (
            SELECT s.account_id FROM favourites f JOIN statuses s ON s.id = f.status_id -- STUBKEEP: interaction aggregate
            UNION ALL
            SELECT s.account_id FROM statuses r JOIN statuses s ON s.id = r.reblog_of_id -- STUBKEEP: interaction aggregate
                WHERE r.reblog_of_id IS NOT NULL
        ) inter ON inter.account_id = a.id
        WHERE a.id <> $1
          AND a.discoverable IS TRUE
          AND a.actor_type IS DISTINCT FROM 'Group'
          AND a.suspended_at IS NULL AND a.silenced_at IS NULL
          AND NOT account_hidden($1, a.id)
          AND NOT EXISTS (
              SELECT 1 FROM follows me
              WHERE me.account_id = $1 AND me.target_account_id = a.id)
          AND NOT EXISTS (
              SELECT 1 FROM follow_recommendation_mutes s
              WHERE s.account_id = $1 AND s.target_account_id = a.id)
        GROUP BY a.id
        ORDER BY count(*) DESC, a.id DESC
        LIMIT $2
        "#,
        viewer,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Records that `viewer` dismissed `target` as a suggestion (idempotent), so it
/// never resurfaces.
pub async fn suppress(pool: &PgPool, viewer: i64, target: i64) -> Result<(), DbError> {
    sqlx::query!(
        r#"
        INSERT INTO follow_recommendation_mutes (account_id, target_account_id)
        VALUES ($1, $2)
        ON CONFLICT (account_id, target_account_id) DO NOTHING
        "#,
        viewer,
        target,
    )
    .execute(pool)
    .await?;
    Ok(())
}
