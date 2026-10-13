//! Lemmy inbox projections over the shared native read state.

use sqlx::PgPool;

use crate::conversation::AccountConversation;
use crate::{DbError, notification::Notification};

pub use crate::notification_read::{mark_all_read, read_states, set_read};

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
