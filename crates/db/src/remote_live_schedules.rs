//! Calendar announcements attached to inbound live Videos. They do not own
//! media or trigger a broadcast; the remote publisher remains the source of
//! truth and each Update replaces the advertised dates.

use std::collections::HashMap;

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

pub async fn replace(
    pool: &PgPool,
    media_id: i64,
    starts_at: &[OffsetDateTime],
) -> Result<bool, DbError> {
    if starts_at.is_empty() {
        return Ok(sqlx::query!(
            "DELETE FROM remote_live_schedules WHERE media_id = $1",
            media_id,
        )
        .execute(pool)
        .await?
        .rows_affected()
            == 1);
    }
    Ok(sqlx::query_scalar!(
        "INSERT INTO remote_live_schedules (media_id, starts_at) VALUES ($1, $2)
         ON CONFLICT (media_id) DO UPDATE SET starts_at = EXCLUDED.starts_at
         WHERE remote_live_schedules.starts_at IS DISTINCT FROM EXCLUDED.starts_at
         RETURNING media_id",
        media_id,
        starts_at,
    )
    .fetch_optional(pool)
    .await?
    .is_some())
}

pub async fn for_media(
    pool: &PgPool,
    media_ids: &[i64],
) -> Result<HashMap<i64, Vec<OffsetDateTime>>, DbError> {
    if media_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query!(
        "SELECT media_id, starts_at FROM remote_live_schedules WHERE media_id = ANY($1)",
        media_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.media_id, row.starts_at))
        .collect())
}
