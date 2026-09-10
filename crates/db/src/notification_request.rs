//! Notification requests — Mastodon's per-sender rollup of *filtered*
//! notifications. A `mention`/`quote` notification stored with
//! `filtered = TRUE` (see [`notification::create`](crate::notification::create))
//! lands in a request keyed by sender; the recipient lists these, then
//! [`accept`]s (unfilter + remember the sender) or [`dismiss`]es (purge) them.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// Kinds that roll up into a request — Mastodon's `update_notification_request!`
/// only fires for `mention` and `quote`.
const REQUESTABLE_KINDS: [&str; 2] = ["mention", "quote"];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NotificationRequest {
    pub id: i64,
    pub account_id: i64,
    pub from_account_id: i64,
    /// Most recent filtered status from the sender (for the request preview).
    pub last_status_id: Option<i64>,
    pub notifications_count: i64,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Records a freshly-filtered `mention`/`quote`: upserts the sender's request,
/// pointing it at `status_id` and recomputing its count. No-op for other kinds.
pub async fn record_filtered(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
    kind: &str,
    status_id: Option<i64>,
) -> Result<(), DbError> {
    if !REQUESTABLE_KINDS.contains(&kind) {
        return Ok(());
    }
    let count = count_filtered(pool, account_id, from_account_id).await?;
    sqlx::query!(
        r#"
        INSERT INTO notification_requests
            (id, account_id, from_account_id, last_status_id, notifications_count)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (account_id, from_account_id) DO UPDATE SET
            last_status_id = EXCLUDED.last_status_id,
            notifications_count = EXCLUDED.notifications_count,
            updated_at = now()
        "#,
        id::next(),
        account_id,
        from_account_id,
        status_id,
        count,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Batched [`record_filtered`]: one upsert refreshes the per-sender request
/// row for every recipient whose just-stored notification was filtered — the
/// per-recipient capped count folds into the statement as a correlated
/// subquery (plan cost, not round trips). `account_ids` must be distinct or
/// the upsert would hit one request row twice in a single statement.
pub async fn record_filtered_many(
    pool: &PgPool,
    account_ids: &[i64],
    from_account_id: i64,
    kind: &str,
    status_id: Option<i64>,
) -> Result<(), DbError> {
    if account_ids.is_empty() || !REQUESTABLE_KINDS.contains(&kind) {
        return Ok(());
    }
    let ids: Vec<i64> = account_ids.iter().map(|_| id::next()).collect();
    sqlx::query!(
        r#"
        INSERT INTO notification_requests
            (id, account_id, from_account_id, last_status_id, notifications_count)
        SELECT v.id, v.account_id, $3, $4,
               (SELECT COUNT(*) FROM (
                    SELECT 1 FROM notifications n
                    WHERE n.account_id = v.account_id AND n.from_account_id = $3
                      AND n.filtered AND n.kind = ANY($5)
                    LIMIT 100
               ) capped)
        FROM unnest($1::bigint[], $2::bigint[]) AS v(id, account_id)
        ON CONFLICT (account_id, from_account_id) DO UPDATE SET
            last_status_id = EXCLUDED.last_status_id,
            notifications_count = EXCLUDED.notifications_count,
            updated_at = now()
        "#,
        &ids,
        account_ids,
        from_account_id,
        status_id,
        &REQUESTABLE_KINDS.map(String::from),
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Count of filtered `mention`/`quote` notifications from one sender, capped at
/// Mastodon's `MAX_MEANINGFUL_COUNT` (100).
async fn count_filtered(
    pool: &PgPool,
    account_id: i64,
    from_account_id: i64,
) -> Result<i64, DbError> {
    let count = sqlx::query_scalar!(
        r#"
        SELECT COUNT(*) AS "count!" FROM (
            SELECT 1 FROM notifications
            WHERE account_id = $1 AND from_account_id = $2 AND filtered
              AND kind = ANY($3)
            LIMIT 100
        ) capped
        "#,
        account_id,
        from_account_id,
        &REQUESTABLE_KINDS.map(String::from),
    )
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// A page of the recipient's requests, newest-first, keyset by id.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<NotificationRequest>, DbError> {
    if let Some(min_id) = min_id {
        let mut rows = sqlx::query_as!(
            NotificationRequest,
            r#"
            SELECT id, account_id, from_account_id, last_status_id,
                   notifications_count, created_at, updated_at
            FROM notification_requests
            WHERE account_id = $1 AND id > $2
              AND ($3::bigint IS NULL OR id < $3)
            ORDER BY id ASC
            LIMIT $4
            "#,
            account_id,
            min_id,
            max_id,
            limit,
        )
        .fetch_all(pool)
        .await?;
        rows.reverse();
        return Ok(rows);
    }
    let rows = sqlx::query_as!(
        NotificationRequest,
        r#"
        SELECT id, account_id, from_account_id, last_status_id,
               notifications_count, created_at, updated_at
        FROM notification_requests
        WHERE account_id = $1
          AND ($2::bigint IS NULL OR id < $2)
          AND ($3::bigint IS NULL OR id > $3)
        ORDER BY id DESC
        LIMIT $4
        "#,
        account_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// A single request owned by `account_id`.
pub async fn find(
    pool: &PgPool,
    account_id: i64,
    id: i64,
) -> Result<Option<NotificationRequest>, DbError> {
    let row = sqlx::query_as!(
        NotificationRequest,
        r#"
        SELECT id, account_id, from_account_id, last_status_id,
               notifications_count, created_at, updated_at
        FROM notification_requests
        WHERE account_id = $1 AND id = $2
        "#,
        account_id,
        id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

/// The subset of `ids` that are requests owned by `account_id` (bulk actions).
pub async fn get_many(
    pool: &PgPool,
    account_id: i64,
    ids: &[i64],
) -> Result<Vec<NotificationRequest>, DbError> {
    let rows = sqlx::query_as!(
        NotificationRequest,
        r#"
        SELECT id, account_id, from_account_id, last_status_id,
               notifications_count, created_at, updated_at
        FROM notification_requests
        WHERE account_id = $1 AND id = ANY($2)
        "#,
        account_id,
        ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Accepts a sender: remembers them as a
/// [permission][crate::notification_policy::permission_exists] so future notifications bypass
/// filtering, unfilters their past notifications, and removes the request. Idempotent.
pub async fn accept(pool: &PgPool, account_id: i64, from_account_id: i64) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let direct_statuses = filtered_direct_statuses(&mut tx, account_id, from_account_id).await?;
    sqlx::query!(
        r#"
        INSERT INTO notification_permissions (id, account_id, from_account_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, from_account_id) DO NOTHING
        "#,
        id::next(),
        account_id,
        from_account_id,
    )
    .execute(&mut *tx)
    .await?;
    backfill_conversations(&mut tx, account_id, &direct_statuses).await?;
    sqlx::query!(
        r#"
        UPDATE notifications SET filtered = FALSE
        WHERE account_id = $1 AND from_account_id = $2 AND filtered
        "#,
        account_id,
        from_account_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM notification_requests WHERE account_id = $1 AND from_account_id = $2"#,
        account_id,
        from_account_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Backfills the sender's filtered DMs into the viewer's conversation rows in
/// one statement. Statuses are grouped by their target row (conversation,
/// participant set) in memory first — two statuses in the same conversation
/// would otherwise make the upsert touch one row twice, which ON CONFLICT
/// DO UPDATE rejects. Within a group the rows arrive ordered by status id,
/// so the group's `unread` is the last status's flag, matching the
/// per-status upsert this replaces.
async fn backfill_conversations(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: i64,
    direct_statuses: &[FilteredDirectStatus],
) -> Result<(), DbError> {
    if direct_statuses.is_empty() {
        return Ok(());
    }
    let mut groups: Vec<ConversationBackfill> = Vec::new();
    for status in direct_statuses {
        let unread = status.sender_id != account_id;
        if let Some(group) = groups.iter_mut().find(|g| {
            g.conversation_id == status.conversation_id
                && g.participant_account_ids == status.participant_account_ids
        }) {
            group.status_ids.push(status.status_id);
            group.unread = unread;
        } else {
            groups.push(ConversationBackfill {
                conversation_id: status.conversation_id,
                participant_account_ids: status.participant_account_ids.clone(),
                status_ids: vec![status.status_id],
                unread,
            });
        }
    }
    let grp_ids: Vec<i64> = groups.iter().map(|_| id::next()).collect();
    let grp_conversations: Vec<i64> = groups.iter().map(|g| g.conversation_id).collect();
    let grp_last: Vec<i64> = groups
        .iter()
        .map(|g| {
            *g.status_ids
                .last()
                .expect("group holds at least one status")
        })
        .collect();
    let grp_unread: Vec<bool> = groups.iter().map(|g| g.unread).collect();
    let mut grp_ords: Vec<i64> = Vec::with_capacity(groups.len());
    let mut part_ords: Vec<i64> = Vec::new();
    let mut part_ids: Vec<i64> = Vec::new();
    let mut sid_ords: Vec<i64> = Vec::new();
    let mut sid_ids: Vec<i64> = Vec::new();
    for (ord, group) in groups.iter().enumerate() {
        let ord = i64::try_from(ord).expect("group count fits in i64");
        grp_ords.push(ord);
        for &participant in &group.participant_account_ids {
            part_ords.push(ord);
            part_ids.push(participant);
        }
        for &status_id in &group.status_ids {
            sid_ords.push(ord);
            sid_ids.push(status_id);
        }
    }
    sqlx::query!(
        r#"
            INSERT INTO account_conversations
                (id, account_id, conversation_id, participant_account_ids,
                 status_ids, last_status_id, unread)
            SELECT g.id, $1, g.conversation_id,
                   coalesce(p.participants, '{}'::bigint[]),
                   s.status_ids, g.last_status_id, g.unread
            FROM unnest($2::bigint[], $3::bigint[], $4::bigint[], $5::bool[], $6::bigint[])
                 AS g(id, conversation_id, last_status_id, unread, ord)
            LEFT JOIN (
                SELECT p.ord, array_agg(p.participant ORDER BY p.participant) AS participants
                FROM unnest($7::bigint[], $8::bigint[]) AS p(ord, participant)
                GROUP BY p.ord
            ) p ON p.ord = g.ord
            JOIN (
                SELECT s.ord, array_agg(s.status_id ORDER BY s.status_id) AS status_ids
                FROM unnest($9::bigint[], $10::bigint[]) AS s(ord, status_id)
                GROUP BY s.ord
            ) s ON s.ord = g.ord
            ON CONFLICT (account_id, conversation_id, participant_account_ids) DO UPDATE SET
                status_ids = account_conversations.status_ids || (
                    SELECT coalesce(array_agg(f.x ORDER BY f.ord), '{}'::bigint[])
                    FROM unnest(excluded.status_ids) WITH ORDINALITY AS f(x, ord)
                    WHERE NOT account_conversations.status_ids @> ARRAY[f.x]
                ),
                last_status_id = GREATEST(account_conversations.last_status_id,
                                          excluded.last_status_id),
                unread = CASE
                    WHEN account_conversations.status_ids @> excluded.status_ids
                    THEN account_conversations.unread
                    ELSE excluded.unread END
            "#,
        account_id,
        &grp_ids,
        &grp_conversations,
        &grp_last,
        &grp_unread,
        &grp_ords,
        &part_ords,
        &part_ids,
        &sid_ords,
        &sid_ids,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Dismisses a sender: deletes their filtered notifications and the request
/// (Mastodon's filtered-notification cleanup job plus dropping the request).
/// Idempotent.
pub async fn dismiss(pool: &PgPool, account_id: i64, from_account_id: i64) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let direct_statuses = filtered_direct_statuses(&mut tx, account_id, from_account_id).await?;
    // Strip every backfilled status from the viewer's conversation rows in one
    // pass; rows left without statuses disappear, as when removing one by one.
    if !direct_statuses.is_empty() {
        let status_ids: Vec<i64> = direct_statuses.iter().map(|s| s.status_id).collect();
        sqlx::query!(
            r#"
            UPDATE account_conversations
            SET status_ids = ARRAY(SELECT t.x
                                   FROM unnest(status_ids) WITH ORDINALITY AS t(x, ord)
                                   WHERE t.x <> ALL($2::bigint[])
                                   ORDER BY t.ord),
                last_status_id = (SELECT max(t.x) FROM unnest(status_ids) AS t(x)
                                  WHERE t.x <> ALL($2::bigint[]))
            WHERE account_id = $1 AND status_ids && $2::bigint[]
            "#,
            account_id,
            &status_ids,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM account_conversations
             WHERE account_id = $1 AND cardinality(status_ids) = 0",
            account_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        r#"
        DELETE FROM notifications
        WHERE account_id = $1 AND from_account_id = $2 AND filtered
        "#,
        account_id,
        from_account_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"DELETE FROM notification_requests WHERE account_id = $1 AND from_account_id = $2"#,
        account_id,
        from_account_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

struct FilteredDirectStatus {
    status_id: i64,
    conversation_id: i64,
    sender_id: i64,
    participant_account_ids: Vec<i64>,
}

async fn filtered_direct_statuses(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: i64,
    from_account_id: i64,
) -> Result<Vec<FilteredDirectStatus>, DbError> {
    let rows = sqlx::query!(
        r#"
        SELECT s.id AS "status_id!",
               sc.conversation_id AS "conversation_id!",
               s.account_id AS "sender_id!",
               ARRAY(
                   SELECT DISTINCT participant_id
                   FROM (
                       SELECT s.account_id AS participant_id
                       UNION
                       SELECT sm.account_id AS participant_id
                       FROM status_mentions sm
                       WHERE sm.status_id = s.id
                   ) participants
                   WHERE participant_id <> $1
                   ORDER BY participant_id
               ) AS "participant_account_ids!"
        FROM notifications n
        JOIN statuses s ON s.id = n.status_id -- STUBKEEP: a stub's notification still names its conversation; rows die with the hard delete
        JOIN status_conversations sc ON sc.status_id = s.id
        WHERE n.account_id = $1
          AND n.from_account_id = $2
          AND n.filtered
          AND n.kind = 'mention'
          AND s.visibility = 'direct'
        ORDER BY s.id ASC
        "#,
        account_id,
        from_account_id,
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| FilteredDirectStatus {
            status_id: row.status_id,
            conversation_id: row.conversation_id,
            sender_id: row.sender_id,
            participant_account_ids: row.participant_account_ids,
        })
        .collect())
}

/// One target `account_conversations` row of a backfill: every filtered direct
/// status that lands in the same (conversation, participant set) row, in
/// status-id order.
struct ConversationBackfill {
    conversation_id: i64,
    participant_account_ids: Vec<i64>,
    status_ids: Vec<i64>,
    unread: bool,
}
