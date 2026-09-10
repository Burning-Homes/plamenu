use std::collections::HashMap;

use sqlx::PgPool;

use crate::DbError;

#[derive(Debug, Clone, Default)]
pub struct Metadata {
    pub alt_text: String,
    pub keywords: Vec<String>,
}

pub async fn set_metadata(
    pool: &PgPool,
    emoji_id: i64,
    alt_text: &str,
    keywords: &[String],
) -> Result<(), DbError> {
    sqlx::query(
        r"
        INSERT INTO lemmy_custom_emoji_metadata (emoji_id, alt_text, keywords)
        VALUES ($1, $2, $3)
        ON CONFLICT (emoji_id) DO UPDATE
        SET alt_text = EXCLUDED.alt_text, keywords = EXCLUDED.keywords
        ",
    )
    .bind(emoji_id)
    .bind(alt_text)
    .bind(keywords)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn metadata_for(
    pool: &PgPool,
    emoji_ids: &[i64],
) -> Result<HashMap<i64, Metadata>, DbError> {
    if emoji_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as::<_, (i64, String, Vec<String>)>(
        "SELECT emoji_id, alt_text, keywords
         FROM lemmy_custom_emoji_metadata WHERE emoji_id = ANY($1)",
    )
    .bind(emoji_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, alt_text, keywords)| (id, Metadata { alt_text, keywords }))
        .collect())
}

/// Replaces an emoji's image/category and metadata in one transaction.
pub async fn update(
    pool: &PgPool,
    emoji_id: i64,
    image: Option<(&str, &str, i64)>,
    category: Option<&str>,
    alt_text: &str,
    keywords: &[String],
) -> Result<bool, DbError> {
    let mut tx = pool.begin().await?;
    let (file, content_type, size) = image.map_or((None, None, None), |(file, kind, size)| {
        (Some(file), Some(kind), Some(size))
    });
    let result = sqlx::query(
        r"
        UPDATE custom_emojis SET
            image_file_name = COALESCE($2, image_file_name),
            image_content_type = COALESCE($3, image_content_type),
            image_file_size = COALESCE($4, image_file_size),
            category = $5,
            disabled = FALSE,
            visible_in_picker = TRUE,
            updated_at = now()
        WHERE id = $1 AND domain IS NULL AND owner_account_id IS NULL
        ",
    )
    .bind(emoji_id)
    .bind(file)
    .bind(content_type)
    .bind(size)
    .bind(category)
    .execute(&mut *tx)
    .await?;
    if result.rows_affected() == 0 {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query(
        r"
        INSERT INTO lemmy_custom_emoji_metadata (emoji_id, alt_text, keywords)
        VALUES ($1, $2, $3)
        ON CONFLICT (emoji_id) DO UPDATE
        SET alt_text = EXCLUDED.alt_text, keywords = EXCLUDED.keywords
        ",
    )
    .bind(emoji_id)
    .bind(alt_text)
    .bind(keywords)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}
