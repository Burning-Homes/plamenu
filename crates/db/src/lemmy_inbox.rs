//! Durable read semantics needed only by the Lemmy compatibility surface.
//!
//! Plamenu normally records one notification high-water mark. Lemmy permits
//! toggling an individual reply/mention out of order, so a sparse override is
//! layered over that native marker without changing native notification APIs.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::conversation::AccountConversation;
use crate::{DbError, notification::Notification};

pub async fn read_states(
    pool: &PgPool,
    user_id: i64,
    account_id: i64,
    notification_ids: &[i64],
) -> Result<HashMap<i64, bool>, DbError> {
    if notification_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (i64, bool)>(
        r"
        SELECT n.id,
               COALESCE(o.read, n.id <= COALESCE(m.last_read_id, 0)) AS read
        FROM notifications n
        LEFT JOIN markers m
          ON m.user_id = $1 AND m.timeline = 'notifications'
        LEFT JOIN lemmy_notification_read_overrides o
          ON o.account_id = $2 AND o.notification_id = n.id
        WHERE n.account_id = $2 AND n.id = ANY($3)
        ",
    )
    .bind(user_id)
    .bind(account_id)
    .bind(notification_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn set_read(
    pool: &PgPool,
    account_id: i64,
    notification_id: i64,
    read: bool,
) -> Result<bool, DbError> {
    let result = sqlx::query(
        r"
        INSERT INTO lemmy_notification_read_overrides (account_id, notification_id, read)
        SELECT $1, id, $3 FROM notifications
        WHERE id = $2 AND account_id = $1
        ON CONFLICT (account_id, notification_id) DO UPDATE SET read = EXCLUDED.read
        ",
    )
    .bind(account_id)
    .bind(notification_id)
    .bind(read)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn mark_all_read(pool: &PgPool, user_id: i64, account_id: i64) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let newest: Option<i64> =
        sqlx::query_scalar("SELECT max(id) FROM notifications WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&mut *tx)
            .await?;
    if let Some(id) = newest {
        sqlx::query(
            r"
            INSERT INTO markers (user_id, timeline, last_read_id, version)
            VALUES ($1, 'notifications', $2, 1)
            ON CONFLICT (user_id, timeline) DO UPDATE
            SET last_read_id = GREATEST(markers.last_read_id, EXCLUDED.last_read_id),
                version = CASE WHEN markers.last_read_id < EXCLUDED.last_read_id
                               THEN markers.version + 1 ELSE markers.version END,
                updated_at = CASE WHEN markers.last_read_id < EXCLUDED.last_read_id
                                  THEN now() ELSE markers.updated_at END
            ",
        )
        .bind(user_id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM lemmy_notification_read_overrides WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn find_notification(
    pool: &PgPool,
    account_id: i64,
    notification_id: i64,
) -> Result<Option<Notification>, DbError> {
    sqlx::query_as::<_, Notification>(
        r"
        SELECT id, account_id, from_account_id, kind, status_id, collection_id,
               emoji, created_at, group_key, filtered, account_warning_id, report_id
        FROM notifications
        WHERE id = $1 AND account_id = $2 AND kind = 'mention' AND NOT filtered
        ",
    )
    .bind(notification_id)
    .bind(account_id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::from)
}

/// The native conversation row through which `account_id` may see one direct
/// status. Lemmy addresses messages while Plamenu addresses threads; this is
/// the owner-scoped bridge used by PM edit/delete/read operations.
pub async fn conversation_for_status(
    pool: &PgPool,
    account_id: i64,
    status_id: i64,
) -> Result<Option<AccountConversation>, DbError> {
    sqlx::query_as::<_, AccountConversation>(
        r"
        SELECT id, account_id, conversation_id, participant_account_ids,
               status_ids, last_status_id, unread
        FROM account_conversations
        WHERE account_id = $1 AND status_ids @> ARRAY[$2::bigint]
        ORDER BY last_status_id DESC
        LIMIT 1
        ",
    )
    .bind(account_id)
    .bind(status_id)
    .fetch_optional(pool)
    .await
    .map_err(DbError::from)
}
