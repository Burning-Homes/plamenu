//! Bookmarks — private status markers, never federated.

use sqlx::PgPool;

use crate::{DbError, id};

/// One row of the bookmarks listing: the bookmark (pagination key) and the
/// status it marks.
#[derive(Debug, Clone, Copy)]
pub struct BookmarkEntry {
    pub row_id: i64,
    pub status_id: i64,
}

/// Records a bookmark; idempotent per (account, status). Returns its row id.
pub async fn create(pool: &PgPool, account_id: i64, status_id: i64) -> Result<i64, DbError> {
    let row_id = sqlx::query_scalar!(
        r#"
        INSERT INTO bookmarks (id, account_id, status_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, status_id) DO UPDATE SET status_id = EXCLUDED.status_id
        RETURNING id
        "#,
        id::next(),
        account_id,
        status_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row_id)
}

/// Removes a bookmark; returns its row id if it existed.
pub async fn delete(
    pool: &PgPool,
    account_id: i64,
    status_id: i64,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        "DELETE FROM bookmarks WHERE account_id = $1 AND status_id = $2 RETURNING id",
        account_id,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id)
}

/// Which of `status_ids` the viewer has bookmarked.
pub async fn bookmarked_of(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT status_id AS "id!" FROM bookmarks
           WHERE account_id = $1 AND status_id = ANY($2)"#,
        account_id,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// [`bookmarked_of`] across a set of viewers in one query — `(viewer, status)`
/// pairs where the viewer bookmarked the status.
pub async fn bookmarked_of_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    status_ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, status_id FROM bookmarks
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

/// An account's bookmarks, newest first, keyset-paginated by bookmark row id
/// (Mastodon paginates the bookmark rows, not the statuses). `min_id`
/// selects the page just above it, presented newest-first.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<BookmarkEntry>, DbError> {
    if let Some(min_id) = min_id {
        let mut entries = sqlx::query_as!(
            BookmarkEntry,
            r#"
            SELECT id AS "row_id!", status_id AS "status_id!"
            FROM bookmarks
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
        entries.reverse();
        return Ok(entries);
    }
    let entries = sqlx::query_as!(
        BookmarkEntry,
        r#"
        SELECT id AS "row_id!", status_id AS "status_id!"
        FROM bookmarks
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
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::status;

    async fn local_account(pool: &PgPool, username: &str) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username,
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn bookmark_lifecycle_and_listing(pool: PgPool) {
        let alice = local_account(&pool, "alice").await;
        let mut posts = Vec::new();
        for n in 0..3 {
            posts.push(
                status::create_local(
                    &pool,
                    status::NewLocalStatus::new(alice, &format!("<p>{n}</p>"), "public", None),
                )
                .await
                .unwrap(),
            );
        }

        let first = create(&pool, alice, posts[0].id).await.unwrap();
        assert_eq!(
            create(&pool, alice, posts[0].id).await.unwrap(),
            first,
            "idempotent"
        );
        let second = create(&pool, alice, posts[1].id).await.unwrap();
        let third = create(&pool, alice, posts[2].id).await.unwrap();

        assert_eq!(
            bookmarked_of(&pool, alice, &[posts[0].id, posts[2].id])
                .await
                .unwrap()
                .len(),
            2
        );

        // Newest bookmark first; max_id pages downward.
        let page = list(&pool, alice, None, None, None, 2).await.unwrap();
        assert_eq!(
            page.iter().map(|e| e.row_id).collect::<Vec<_>>(),
            [third, second]
        );
        let next = list(&pool, alice, Some(second), None, None, 2)
            .await
            .unwrap();
        assert_eq!(next.iter().map(|e| e.row_id).collect::<Vec<_>>(), [first]);

        // min_id selects the adjacent newer page, presented newest-first.
        let prev = list(&pool, alice, None, None, Some(first), 2)
            .await
            .unwrap();
        assert_eq!(
            prev.iter().map(|e| e.row_id).collect::<Vec<_>>(),
            [third, second]
        );

        assert_eq!(
            delete(&pool, alice, posts[0].id).await.unwrap(),
            Some(first)
        );
        assert_eq!(delete(&pool, alice, posts[0].id).await.unwrap(), None);

        // Deleting the status cascades the bookmark away.
        status::delete_local(&pool, posts[2].id, alice)
            .await
            .unwrap();
        let remaining = list(&pool, alice, None, None, None, 10).await.unwrap();
        assert_eq!(
            remaining.iter().map(|e| e.row_id).collect::<Vec<_>>(),
            [second]
        );
    }
}
