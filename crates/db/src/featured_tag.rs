//! Featured hashtags — Mastodon's `featured_tags`. An account pins hashtags to
//! its profile; each is exposed with the account's usage stats for that tag.
//! Featuring a tag federates as `Add(Hashtag)`, so remote accounts get rows
//! here too.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// A featured tag with the owner's usage stats for it, as the
/// `FeaturedTag` entity needs them.
pub struct FeaturedTag {
    pub id: i64,
    pub tag_id: i64,
    pub name: String,
    pub statuses_count: i64,
    pub last_status_at: Option<OffsetDateTime>,
}

/// A tag the account has used but not featured — a featuring suggestion.
pub struct TagSuggestion {
    pub tag_id: i64,
    pub name: String,
    pub display_name: Option<String>,
}

/// Features `tag_id` on `account_id`'s profile, returning the new (or existing)
/// featured-tag row id. Idempotent on the `(account, tag)` pair.
pub async fn feature<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    tag_id: i64,
) -> Result<i64, DbError> {
    let row_id = sqlx::query_scalar!(
        r#"
        INSERT INTO featured_tags (id, account_id, tag_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, tag_id) DO UPDATE SET account_id = featured_tags.account_id
        RETURNING id
        "#,
        id::next(),
        account_id,
        tag_id,
    )
    .fetch_one(pool)
    .await?;
    Ok(row_id)
}

/// Unfeatures `tag_id` from `account_id`'s profile, returning whether a row was
/// removed.
pub async fn unfeature<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    tag_id: i64,
) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM featured_tags WHERE account_id = $1 AND tag_id = $2",
        account_id,
        tag_id,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Removes a featured tag by its own row id (the REST `DELETE` path), returning
/// the unfeatured `tag_id` so the caller can federate the `Remove`. `None` when
/// the id is unknown or owned by another account.
pub async fn delete_by_id<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    featured_tag_id: i64,
) -> Result<Option<i64>, DbError> {
    let tag_id = sqlx::query_scalar!(
        "DELETE FROM featured_tags WHERE id = $1 AND account_id = $2 RETURNING tag_id",
        featured_tag_id,
        account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(tag_id)
}

/// Whether `account_id` features `tag_id`.
pub async fn exists(pool: &PgPool, account_id: i64, tag_id: i64) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one!" FROM featured_tags WHERE account_id = $1 AND tag_id = $2"#,
        account_id,
        tag_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Of `tag_ids`, which ones `account_id` features — for the `featuring` flag on
/// a batch of `Tag` entities (search results, followed tags).
pub async fn featured_ids(
    pool: &PgPool,
    account_id: i64,
    tag_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT tag_id AS "tag_id!" FROM featured_tags
           WHERE account_id = $1 AND tag_id = ANY($2)"#,
        account_id,
        tag_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// `account_id`'s featured tags, most recently featured first, each carrying
/// the account's count of (and latest) distributable status using the tag.
pub async fn list<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
) -> Result<Vec<FeaturedTag>, DbError> {
    let rows = sqlx::query_as!(
        FeaturedTag,
        r#"
        SELECT ft.id AS "id!", ft.tag_id AS "tag_id!", t.name AS "name!",
               count(s.id) AS "statuses_count!",
               max(s.created_at) AS "last_status_at"
        FROM featured_tags ft
        JOIN tags t ON t.id = ft.tag_id
        LEFT JOIN status_tags st ON st.tag_id = ft.tag_id
        LEFT JOIN statuses s -- STUBKEEP: own-tag aggregate; a stub still witnesses the tag was used
            ON s.id = st.status_id
           AND s.account_id = ft.account_id
           AND s.visibility IN ('public', 'unlisted')
        WHERE ft.account_id = $1
        GROUP BY ft.id, t.name
        ORDER BY count(s.id) DESC, ft.id DESC
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Tags `account_id` has used in its own statuses but not featured, most
/// recently used first — Mastodon's featuring suggestions (capped at 10).
pub async fn suggestions(pool: &PgPool, account_id: i64) -> Result<Vec<TagSuggestion>, DbError> {
    let rows = sqlx::query_as!(
        TagSuggestion,
        r#"
        SELECT t.id AS "tag_id!", t.name AS "name!", t.display_name
        FROM status_tags st
        JOIN statuses s ON s.id = st.status_id AND s.account_id = $1 -- STUBKEEP: own-tag aggregate; a stub still witnesses the tag was used
        JOIN tags t ON t.id = st.tag_id
        WHERE NOT EXISTS (
            SELECT 1 FROM featured_tags ft
            WHERE ft.account_id = $1 AND ft.tag_id = t.id
        )
        GROUP BY t.id, t.name, t.display_name
        ORDER BY max(s.created_at) DESC
        LIMIT 10
        "#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::tag;

    async fn local(pool: &PgPool, username: &str) -> i64 {
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
    async fn feature_unfeature_and_list(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let rust = tag::ensure(&pool, "rust").await.unwrap();
        let art = tag::ensure(&pool, "art").await.unwrap();

        assert!(!exists(&pool, alice, rust).await.unwrap());
        feature(&pool, alice, rust).await.unwrap();
        feature(&pool, alice, art).await.unwrap();
        // Idempotent.
        feature(&pool, alice, rust).await.unwrap();
        assert!(exists(&pool, alice, rust).await.unwrap());

        let listed = list(&pool, alice).await.unwrap();
        // Most recently featured first; no statuses yet.
        assert_eq!(
            listed.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["art", "rust"]
        );
        assert_eq!(listed[0].statuses_count, 0);
        assert!(listed[0].last_status_at.is_none());

        assert!(unfeature(&pool, alice, art).await.unwrap());
        assert!(!unfeature(&pool, alice, art).await.unwrap());
        assert_eq!(list(&pool, alice).await.unwrap().len(), 1);

        // Deleting by row id returns the freed tag id.
        let row = feature(&pool, alice, art).await.unwrap();
        assert_eq!(delete_by_id(&pool, alice, row).await.unwrap(), Some(art));
        assert_eq!(delete_by_id(&pool, alice, row).await.unwrap(), None);
    }
}
