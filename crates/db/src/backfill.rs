//! One-shot backfills for columns whose producers were added after rows
//! already existed. Currently just the media byte sizes that back the admin
//! `space_usage` / `instance_media_attachments` metrics: the file
//! bytes live on the media store, so the caller stats each stored file and
//! feeds the size back through the setters here.
//!
//! Target selection is keyset-paged rather than a single unbounded
//! `fetch_all`: a class that is missing sizes for millions of
//! rows must not materialize them all in one Rust vector before the sweep can
//! begin. Each page is `id > after_id ORDER BY id LIMIT n`, so the caller
//! walks the class in bounded batches and the id cursor steps past rows that
//! stay `NULL` (an unreadable file the sweep had to skip) instead of
//! re-fetching them forever. Because a filled row leaves the `… IS NULL`
//! predicate, the predicate is itself the durable progress marker: a sweep
//! interrupted by a crash or restart resumes by re-scanning from the lowest
//! still-missing id — an index scan that skips already-filled rows — so no
//! separate cursor needs to be persisted.

use sqlx::PgPool;

use crate::DbError;

/// A stored file (and optional thumbnail) whose byte size is still unknown.
#[derive(Debug, Clone)]
pub struct SizingTarget {
    pub id: i64,
    pub file_name: String,
    /// The preview/thumbnail style, when the row has one.
    pub thumb_file_name: Option<String>,
}

/// One keyset page of media attachments with a stored file but no recorded
/// `file_size` (local uploads and cached remote media alike — both occupy our
/// disk): rows with `id > after_id`, oldest id first, at most `limit` of them.
/// Pass `after_id = 0` for the first page and the last returned `id` for each
/// subsequent page; a short page ends the walk.
pub async fn media_missing_sizes_page(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<SizingTarget>, DbError> {
    let rows = sqlx::query_as!(
        SizingTarget,
        r#"
        SELECT id, file_name AS "file_name!", small_file_name AS "thumb_file_name"
        FROM media_attachments
        WHERE file_name IS NOT NULL AND file_size IS NULL AND id > $1
        ORDER BY id
        LIMIT $2
        "#,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One keyset page of accounts with a stored avatar file but no recorded
/// `avatar_file_size`. See [`media_missing_sizes_page`] for the cursor
/// contract.
pub async fn avatars_missing_sizes_page(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<SizingTarget>, DbError> {
    let rows = sqlx::query_as!(
        SizingTarget,
        r#"
        SELECT id, avatar_file_name AS "file_name!", NULL::text AS "thumb_file_name"
        FROM accounts
        WHERE avatar_file_name IS NOT NULL AND avatar_file_size IS NULL AND id > $1
        ORDER BY id
        LIMIT $2
        "#,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One keyset page of accounts with a stored header file but no recorded
/// `header_file_size`. See [`media_missing_sizes_page`] for the cursor
/// contract.
pub async fn headers_missing_sizes_page(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<SizingTarget>, DbError> {
    let rows = sqlx::query_as!(
        SizingTarget,
        r#"
        SELECT id, header_file_name AS "file_name!", NULL::text AS "thumb_file_name"
        FROM accounts
        WHERE header_file_name IS NOT NULL AND header_file_size IS NULL AND id > $1
        ORDER BY id
        LIMIT $2
        "#,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// One keyset page of custom emoji with a stored image but no recorded
/// `image_file_size`. See [`media_missing_sizes_page`] for the cursor
/// contract.
pub async fn emoji_missing_sizes_page(
    pool: &PgPool,
    after_id: i64,
    limit: i64,
) -> Result<Vec<SizingTarget>, DbError> {
    let rows = sqlx::query_as!(
        SizingTarget,
        r#"
        SELECT id, image_file_name AS "file_name!", NULL::text AS "thumb_file_name"
        FROM custom_emojis
        WHERE image_file_name IS NOT NULL AND image_file_size IS NULL AND id > $1
        ORDER BY id
        LIMIT $2
        "#,
        after_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Records an avatar's stored byte size.
pub async fn set_avatar_size(pool: &PgPool, id: i64, size: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET avatar_file_size = $2 WHERE id = $1",
        id,
        size,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a header's stored byte size.
pub async fn set_header_size(pool: &PgPool, id: i64, size: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE accounts SET header_file_size = $2 WHERE id = $1",
        id,
        size,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Records a custom emoji's stored byte size.
pub async fn set_emoji_size(pool: &PgPool, id: i64, size: i64) -> Result<(), DbError> {
    sqlx::query!(
        "UPDATE custom_emojis SET image_file_size = $2 WHERE id = $1",
        id,
        size,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// [`set_avatar_size`] over a whole page in one statement.
pub async fn set_avatar_sizes(pool: &PgPool, ids: &[i64], sizes: &[i64]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE accounts a SET avatar_file_size = v.size
         FROM unnest($1::bigint[], $2::bigint[]) AS v(id, size)
         WHERE a.id = v.id",
        ids,
        sizes,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// [`set_header_size`] over a whole page in one statement.
pub async fn set_header_sizes(pool: &PgPool, ids: &[i64], sizes: &[i64]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE accounts a SET header_file_size = v.size
         FROM unnest($1::bigint[], $2::bigint[]) AS v(id, size)
         WHERE a.id = v.id",
        ids,
        sizes,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// [`set_emoji_size`] over a whole page in one statement.
pub async fn set_emoji_sizes(pool: &PgPool, ids: &[i64], sizes: &[i64]) -> Result<(), DbError> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE custom_emojis e SET image_file_size = v.size
         FROM unnest($1::bigint[], $2::bigint[]) AS v(id, size)
         WHERE e.id = v.id",
        ids,
        sizes,
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};
    use crate::media::{self, NewLocalMedia};

    async fn seed_account(pool: &PgPool) -> i64 {
        account::create_local(
            pool,
            NewLocalAccount {
                username: "alice",
                display_name: "",
                note: "",
                public_key_pem: "pub",
            },
        )
        .await
        .unwrap()
        .id
    }

    /// Inserts a media row with a stored file but no size, and returns its id.
    async fn seed_sizeless_media(pool: &PgPool, account_id: i64) -> i64 {
        let id = crate::id::next();
        media::create_local(
            pool,
            NewLocalMedia::new(account_id, id, "pic.png", "image/png"),
        )
        .await
        .unwrap();
        id
    }

    /// The paged selector honours its `limit`, walks the whole class by id
    /// cursor, and never returns a row at or before `after_id` — so a sweep
    /// reads the backlog in bounded batches instead of one unbounded fetch.
    #[sqlx::test]
    async fn missing_sizes_pages_are_bounded_and_ordered(pool: PgPool) {
        let account_id = seed_account(&pool).await;
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(seed_sizeless_media(&pool, account_id).await);
        }
        ids.sort_unstable();

        // A limit of 2 yields exactly the two lowest ids.
        let first = media_missing_sizes_page(&pool, 0, 2).await.unwrap();
        assert_eq!(
            first.iter().map(|t| t.id).collect::<Vec<_>>(),
            ids[0..2].to_vec()
        );

        // Walking the cursor visits every row once, in id order, with no page
        // longer than the limit.
        let mut after = 0;
        let mut seen = Vec::new();
        loop {
            let page = media_missing_sizes_page(&pool, after, 2).await.unwrap();
            assert!(page.len() <= 2);
            let Some(last) = page.last() else { break };
            after = last.id;
            seen.extend(page.iter().map(|t| t.id));
        }
        assert_eq!(seen, ids);
    }

    /// A row that stays sizeless (its file could not be read, so the sweep
    /// skipped it) is stepped over by the id cursor rather than re-fetched
    /// forever: advancing `after_id` past it excludes it from later pages, so
    /// the walk terminates.
    #[sqlx::test]
    async fn cursor_steps_past_a_still_missing_row(pool: PgPool) {
        let account_id = seed_account(&pool).await;
        let stuck = seed_sizeless_media(&pool, account_id).await;
        let later = seed_sizeless_media(&pool, account_id).await;

        // Simulate the sweep skipping `stuck` (still NULL) and advancing the
        // cursor past it: the next page must yield only the later row, not the
        // stuck one again.
        let next = media_missing_sizes_page(&pool, stuck, 10).await.unwrap();
        assert_eq!(next.iter().map(|t| t.id).collect::<Vec<_>>(), vec![later]);

        // And past the last id the walk is empty — it terminates even with a
        // permanently sizeless row present.
        assert!(
            media_missing_sizes_page(&pool, later, 10)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
