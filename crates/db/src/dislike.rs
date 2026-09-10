//! Downvotes (`ActivityPub` `Dislike`s) on group posts. The upvote
//! side is `favourite`; a status's score is favourites minus dislikes.

use sqlx::PgPool;

use crate::{DbError, Upserted, id};

/// Records a downvote; idempotent per (account, status) — a repeat keeps the
/// original row and only refreshes the stored `Dislike` activity uri. The
/// result says whether this call inserted the row, so redelivered `Dislike`s
/// don't re-announce.
pub async fn create<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
    uri: Option<&str>,
) -> Result<Upserted, DbError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO status_dislikes (id, account_id, status_id, uri)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, status_id) DO UPDATE SET uri = EXCLUDED.uri
        RETURNING id, (xmax = 0) AS "inserted!"
        "#,
        id::next(),
        account_id,
        status_id,
        uri,
    )
    .fetch_one(pool)
    .await?;
    Ok(Upserted {
        id: row.id,
        inserted: row.inserted,
    })
}

/// Removes a downvote; returns its row id (the `Dislike` marker) if it
/// existed.
pub async fn delete<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        "DELETE FROM status_dislikes WHERE account_id = $1 AND status_id = $2 RETURNING id",
        account_id,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id)
}

/// Removes a downvote by its stored `Dislike` activity uri — the fallback
/// for an inbound `Undo` that names the activity instead of embedding it.
/// Returns the downvoted status id if it existed.
pub async fn delete_by_uri(
    pool: &PgPool,
    account_id: i64,
    uri: &str,
) -> Result<Option<i64>, DbError> {
    let status_id = sqlx::query_scalar!(
        "DELETE FROM status_dislikes WHERE account_id = $1 AND uri = $2 RETURNING status_id",
        account_id,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status_id)
}

/// Which of `status_ids` the viewer has downvoted.
pub async fn disliked_of(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT status_id AS "id!" FROM status_dislikes
           WHERE account_id = $1 AND status_id = ANY($2)"#,
        account_id,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// [`disliked_of`] across a set of viewers in one query — `(viewer, status)`
/// pairs where the viewer downvoted the status.
pub async fn disliked_of_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    status_ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, status_id FROM status_dislikes
           WHERE account_id = ANY($1) AND status_id = ANY($2)"#,
        viewer_ids,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.account_id, row.status_id))
        .collect())
}
