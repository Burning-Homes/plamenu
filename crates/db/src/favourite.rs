//! Favourites (`ActivityPub` `Like`s).

use sqlx::PgPool;

use crate::{DbError, Upserted, id};

/// Records a favourite; idempotent per (account, status) — a repeat keeps the
/// original row and only refreshes the stored `Like` activity uri. The result
/// says whether this call inserted the row, so redelivered `Like`s don't
/// re-notify.
pub async fn create<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
    uri: Option<&str>,
) -> Result<Upserted, DbError> {
    let row = sqlx::query!(
        r#"
        INSERT INTO favourites (id, account_id, status_id, uri)
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

/// Removes a favourite; returns its row id (the `Like` marker) if it existed.
pub async fn delete<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    status_id: i64,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        "DELETE FROM favourites WHERE account_id = $1 AND status_id = $2 RETURNING id",
        account_id,
        status_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id)
}

/// Removes a favourite the way an `Undo` that *embeds* the original `Like`
/// names it: the row must still be the one that `Like` created. A row
/// recorded under a different activity belongs to a later `Like` and is left
/// alone — the sender favourited again, and the `Undo` of the earlier one
/// only overtook it in flight. Returns the removed row's id.
///
/// A row with no recorded uri predates nothing in particular (the `Like`
/// carried no id), so there is no disagreement to act on and it is removed.
pub async fn delete_activity(
    pool: &PgPool,
    account_id: i64,
    status_id: i64,
    uri: &str,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        "DELETE FROM favourites
         WHERE account_id = $1 AND status_id = $2 AND (uri IS NULL OR uri = $3)
         RETURNING id",
        account_id,
        status_id,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row_id)
}

/// Removes a favourite by its stored `Like` activity uri — the fallback for
/// an inbound `Undo` that names the activity instead of embedding it
/// (Pleroma's wire shape). Returns the favourited status id if it existed.
pub async fn delete_by_uri(
    pool: &PgPool,
    account_id: i64,
    uri: &str,
) -> Result<Option<i64>, DbError> {
    let status_id = sqlx::query_scalar!(
        "DELETE FROM favourites WHERE account_id = $1 AND uri = $2 RETURNING status_id",
        account_id,
        uri,
    )
    .fetch_optional(pool)
    .await?;
    Ok(status_id)
}

/// Which of `status_ids` the viewer has favourited.
pub async fn favourited_of(
    pool: &PgPool,
    account_id: i64,
    status_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT status_id AS "id!" FROM favourites
           WHERE account_id = $1 AND status_id = ANY($2)"#,
        account_id,
        status_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// [`favourited_of`] across a set of viewers in one query — `(viewer, status)`
/// pairs where the viewer favourited the status. The streaming fan-out
/// renderer resolves every recipient's flags in one round trip.
pub async fn favourited_of_viewers(
    pool: &PgPool,
    viewer_ids: &[i64],
    status_ids: &[i64],
) -> Result<Vec<(i64, i64)>, DbError> {
    let rows = sqlx::query!(
        r#"SELECT account_id, status_id FROM favourites
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

/// One row of the favourites listing: the favourite (pagination key) and the
/// status it marks.
#[derive(Debug, Clone, Copy)]
pub struct FavouriteEntry {
    pub row_id: i64,
    pub status_id: i64,
}

/// An account's favourites, newest first, keyset-paginated by favourite row
/// id (Mastodon paginates the favourite rows, not the statuses). `min_id`
/// selects the page just above it, presented newest-first.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<FavouriteEntry>, DbError> {
    if let Some(min_id) = min_id {
        let mut entries = sqlx::query_as!(
            FavouriteEntry,
            r#"
            SELECT id AS "row_id!", status_id AS "status_id!"
            FROM favourites
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
        FavouriteEntry,
        r#"
        SELECT id AS "row_id!", status_id AS "status_id!"
        FROM favourites
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

/// One entry of a status's `favourited_by` listing: the favourite row
/// (pagination key) and who favourited.
#[derive(Debug, Clone, Copy)]
pub struct FaverEntry {
    pub row_id: i64,
    pub account_id: i64,
}

/// Accounts that favourited a status, newest favourite first, keyset-
/// paginated by favourite row id. Accounts hidden from the viewer (a block
/// in either direction, or an active mute) are excluded, like Mastodon's
/// `not_excluded_by_account`; an anonymous viewer sees everyone.
pub async fn favers_of(
    pool: &PgPool,
    status_id: i64,
    viewer: Option<i64>,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<FaverEntry>, DbError> {
    let entries = sqlx::query_as!(
        FaverEntry,
        r#"
        SELECT f.id AS "row_id!", f.account_id AS "account_id!"
        FROM favourites f
        WHERE f.status_id = $1
          AND ($2::bigint IS NULL OR f.id < $2)
          AND ($3::bigint IS NULL OR f.id > $3)
          AND NOT account_hidden($4, f.account_id)
        ORDER BY f.id DESC
        LIMIT $5
        "#,
        status_id,
        max_id,
        since_id,
        viewer,
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

    #[sqlx::test]
    async fn favourite_lifecycle(pool: PgPool) {
        let alice = account::create_local(
            &pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap();
        let post = status::create_local(
            &pool,
            status::NewLocalStatus::new(alice.id, "<p>x</p>", "public", None),
        )
        .await
        .unwrap();

        let first = create(&pool, alice.id, post.id, None).await.unwrap();
        assert!(first.inserted);
        let second = create(&pool, alice.id, post.id, Some("https://x/likes/1"))
            .await
            .unwrap();
        assert_eq!(first.id, second.id, "idempotent");
        assert!(!second.inserted, "repeat is flagged as such");
        assert_eq!(
            favourited_of(&pool, alice.id, &[post.id]).await.unwrap(),
            [post.id]
        );

        assert_eq!(
            delete(&pool, alice.id, post.id).await.unwrap(),
            Some(first.id)
        );
        assert_eq!(delete(&pool, alice.id, post.id).await.unwrap(), None);
        assert!(
            favourited_of(&pool, alice.id, &[post.id])
                .await
                .unwrap()
                .is_empty()
        );
        // Favouriting again after an undo is a fresh interaction.
        assert!(
            create(&pool, alice.id, post.id, None)
                .await
                .unwrap()
                .inserted
        );
    }
}
