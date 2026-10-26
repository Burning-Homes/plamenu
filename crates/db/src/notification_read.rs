//! Explicit inbox read state shared by native pages and Lemmy clients.
//! The sparse legacy-named override table preserves existing markers and IDs.
use crate::{DbError, notification::Notification};
use sqlx::PgPool;
use std::collections::HashMap;

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
    Ok(set_many(pool, account_id, &[notification_id], read).await? > 0)
}

pub async fn mark_all_read(pool: &PgPool, user_id: i64, account_id: i64) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE account_conversations SET unread=FALSE WHERE account_id=$1 AND unread")
        .bind(account_id)
        .execute(&mut *tx)
        .await?;
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

/// The activity inbox reads only through the head observed when it was opened.
pub async fn activity_head(pool: &PgPool, account_id: i64) -> Result<Option<i64>, DbError> {
    Ok(
        sqlx::query_scalar("SELECT max(n.id) FROM notifications n WHERE n.account_id=$1")
            .bind(account_id)
            .fetch_one(pool)
            .await?,
    )
}

/// Read notifications through the observed head, including private-post activity.
/// Conversation previews remain unread until their thread is opened. Newer
/// notifications and overrides remain untouched, even if an API client has
/// already advanced the marker beyond this page's snapshot.
pub async fn mark_activity_read(
    pool: &PgPool,
    user_id: i64,
    account_id: i64,
    through: i64,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    // ON CONFLICT locks the marker even when WHERE prevents an update. Keep
    // the following delete as a separate statement: its snapshot must begin
    // after acquiring that lock, including when another page held it first.
    sqlx::query(
        "INSERT INTO markers(user_id,timeline,last_read_id,version)
         VALUES($1,'notifications',GREATEST(0::bigint,$2),CASE WHEN $2>0 THEN 1 ELSE 0 END)
         ON CONFLICT(user_id,timeline) DO UPDATE
         SET last_read_id=$2,version=markers.version+1,updated_at=now()
         WHERE markers.last_read_id<$2",
    )
    .bind(user_id)
    .bind(through)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM lemmy_notification_read_overrides o USING notifications n
        WHERE o.account_id=$1 AND n.account_id=$1 AND o.notification_id=n.id AND n.id<=$2",
    )
    .bind(account_id)
    .bind(through)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Bounded owner-only bulk operation, including mixed/unknown IDs as no-ops.
pub async fn set_many(
    pool: &PgPool,
    account: i64,
    ids: &[i64],
    read: bool,
) -> Result<u64, DbError> {
    let mut tx = pool.begin().await?;
    // Use the same conversation -> notification lock order as opening a thread.
    sqlx::query("SELECT ac.id FROM account_conversations ac WHERE ac.account_id=$1 AND EXISTS (SELECT 1 FROM notifications n WHERE n.account_id=$1 AND n.id=ANY($2) AND n.status_id=ANY(ac.status_ids)) ORDER BY ac.id FOR UPDATE")
        .bind(account).bind(ids).fetch_all(&mut *tx).await?;
    let updated = sqlx::query("INSERT INTO lemmy_notification_read_overrides(account_id,notification_id,read) SELECT $1,id,$3 FROM notifications WHERE account_id=$1 AND id=ANY($2) ON CONFLICT(account_id,notification_id) DO UPDATE SET read=EXCLUDED.read")
        .bind(account).bind(ids).bind(read).execute(&mut *tx).await?.rows_affected();
    sqlx::query(r"UPDATE account_conversations ac SET unread = NOT $3 OR EXISTS (
        SELECT 1 FROM notifications n JOIN users u ON u.account_id=n.account_id
        LEFT JOIN markers m ON m.user_id=u.id AND m.timeline='notifications'
        LEFT JOIN lemmy_notification_read_overrides o ON o.account_id=n.account_id AND o.notification_id=n.id
        WHERE n.account_id=$1 AND n.status_id=ANY(ac.status_ids)
          AND NOT COALESCE(o.read,n.id<=COALESCE(m.last_read_id,0)))
        WHERE ac.account_id=$1 AND EXISTS (SELECT 1 FROM notifications changed
          WHERE changed.account_id=$1 AND changed.id=ANY($2) AND changed.status_id=ANY(ac.status_ids))")
        .bind(account).bind(ids).bind(read).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(updated)
}

#[derive(Default)]
pub struct Filter<'a> {
    pub before: Option<i64>,
    pub unread: bool,
    pub mail: bool,
    pub exclude_direct: bool,
    pub kinds: Option<&'a [String]>,
}
/// Physical keyset pagination applies read/type/policy filters before limiting.
pub async fn list(
    pool: &PgPool,
    user: i64,
    account: i64,
    filter: Filter<'_>,
) -> Result<Vec<Notification>, DbError> {
    sqlx::query_as::<_, Notification>(
        r"
        -- STUBKEEP: classify direct visibility even for stubs; notification_visible separately gates notification access.
        WITH delivery_kinds AS MATERIALIZED (
            SELECT ARRAY(SELECT kind FROM unnest(ARRAY['mention','status','reblog','follow','follow_request','favourite','music.favourite','music.share','music.download','webmention','poll','update','severed_relationships','moderation_warning','annual_report','admin.sign_up','admin.report','quote','quoted_update','live','added_to_collection','collection_update','pleroma:emoji_reaction','event.participation','event.accepted','event.rejected','event.changed','event.invite','plamenu:actor.follow','plamenu:actor.follow_request','plamenu:actor.favourite','plamenu:actor.reblog','plamenu:actor.emoji_reaction','plamenu:actor.comment','plamenu:actor.mention','plamenu:actor.quote','plamenu:actor.status','plamenu:actor.invitation','plamenu:actor.transfer']::text[]) k(kind)
                WHERE ($5::text[] IS NULL OR kind=ANY($5))
                AND CASE WHEN $6 THEN EXISTS (
                    SELECT 1 FROM notification_delivery_preferences p
                    WHERE p.account_id=$2 AND p.kind=k.kind AND (p.email OR p.email_digest)
                ) ELSE NOT EXISTS (
                    SELECT 1 FROM notification_delivery_preferences p
                    WHERE p.account_id=$2 AND p.kind=k.kind AND NOT p.in_app
                ) END) AS kinds
        )
        SELECT n.id,n.account_id,n.from_account_id,
               notification_kind_for(n.reasons,(SELECT kinds FROM delivery_kinds)) AS kind,
               n.status_id,n.collection_id,n.emoji,n.created_at,n.group_key,
               n.filtered,n.account_warning_id,n.report_id
        FROM notifications n
        LEFT JOIN markers m ON m.user_id=$1 AND m.timeline='notifications'
        LEFT JOIN lemmy_notification_read_overrides o ON o.account_id=$2 AND o.notification_id=n.id
        WHERE n.account_id=$2 AND ($3::bigint IS NULL OR n.id<$3)
          AND (NOT $4 OR NOT COALESCE(o.read,n.id<=COALESCE(m.last_read_id,0)))
          AND notification_kind_for(n.reasons,(SELECT kinds FROM delivery_kinds)) IS NOT NULL
          AND NOT n.filtered AND notification_visible(n.account_id,n.id,n.report_id) AND NOT sender_filtered($2,n.from_account_id)
          AND (NOT $7 OR NOT EXISTS (SELECT 1 FROM statuses s WHERE s.id=n.status_id AND s.visibility='direct'))
        ORDER BY n.id DESC LIMIT 41
    ",
    )
    .bind(user)
    .bind(account)
    .bind(filter.before)
    .bind(filter.unread)
    .bind(filter.kinds)
    .bind(filter.mail)
    .bind(filter.exclude_direct)
    .fetch_all(pool)
    .await
    .map_err(DbError::from)
}

pub async fn delete_many(pool: &PgPool, account: i64, ids: &[i64]) -> Result<u64, DbError> {
    Ok(
        sqlx::query("DELETE FROM notifications WHERE account_id=$1 AND id=ANY($2)")
            .bind(account)
            .bind(ids)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}
/// Seen-only retention is bounded per tick and independent of notification
/// preferences. Explicit unread below the marker must never be purged.
pub async fn purge_seen(pool: &PgPool, days: i32) -> Result<u64, DbError> {
    if !(1..=365_000).contains(&days) {
        return Ok(0);
    }
    let result=sqlx::query(r"
        DELETE FROM notifications WHERE id IN (
            SELECT n.id FROM notifications n JOIN users u ON u.account_id=n.account_id
            LEFT JOIN markers m ON m.user_id=u.id AND m.timeline='notifications'
            LEFT JOIN lemmy_notification_read_overrides o ON o.account_id=n.account_id AND o.notification_id=n.id
            WHERE n.created_at<now()-make_interval(days=>$1)
              AND COALESCE(o.read,n.id<=COALESCE(m.last_read_id,0))
            ORDER BY n.created_at,n.id LIMIT 2000 FOR UPDATE OF n SKIP LOCKED
        )
    ").bind(days).execute(pool).await?;
    Ok(result.rows_affected())
}
