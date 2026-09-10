//! Operator-uploaded site images — Mastodon's `SiteUpload` (`thumbnail`,
//! `mascot`, `favicon`, `app_icon`), each with the resized styles Mastodon
//! derives on upload. One row per slot; replacing a slot swaps the row and
//! its variants in one transaction and reports the storage files the caller
//! must delete.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::DbError;

/// The four slots Mastodon models (`SiteUpload::STYLES` keys).
pub const VARS: &[&str] = &["thumbnail", "mascot", "favicon", "app_icon"];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SiteUpload {
    pub var: String,
    pub file_name: String,
    pub content_type: String,
    pub file_size: i64,
    pub width: i32,
    pub height: i32,
    pub blurhash: Option<String>,
    pub description: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SiteUploadVariant {
    pub var: String,
    pub style: String,
    pub file_name: String,
    pub width: i32,
    pub height: i32,
}

/// A new resized style accompanying an upload.
#[derive(Debug, Clone)]
pub struct NewVariant {
    pub style: String,
    pub file_name: String,
    pub width: i32,
    pub height: i32,
}

/// One slot, or `None` when nothing is uploaded.
pub async fn get(pool: &PgPool, var: &str) -> Result<Option<SiteUpload>, DbError> {
    let upload = sqlx::query_as!(
        SiteUpload,
        r#"
        SELECT var, file_name, content_type, file_size, width, height, blurhash,
               description, created_at, updated_at
        FROM site_uploads
        WHERE var = $1
        "#,
        var,
    )
    .fetch_optional(pool)
    .await?;
    Ok(upload)
}

/// Every filled slot, in `VARS` display order.
pub async fn all(pool: &PgPool) -> Result<Vec<SiteUpload>, DbError> {
    let uploads = sqlx::query_as!(
        SiteUpload,
        r#"
        SELECT var, file_name, content_type, file_size, width, height, blurhash,
               description, created_at, updated_at
        FROM site_uploads
        ORDER BY array_position($1::text[], var)
        "#,
        &VARS.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await?;
    Ok(uploads)
}

/// The resized styles of one slot, smallest first.
pub async fn variants_for(pool: &PgPool, var: &str) -> Result<Vec<SiteUploadVariant>, DbError> {
    let variants = sqlx::query_as!(
        SiteUploadVariant,
        r#"
        SELECT var, style, file_name, width, height
        FROM site_upload_variants
        WHERE var = $1
        ORDER BY width, style
        "#,
        var,
    )
    .fetch_all(pool)
    .await?;
    Ok(variants)
}

/// Replaces (or fills) a slot and its variants, returning the storage file
/// names of the previous upload so the caller can delete them after commit.
#[allow(clippy::too_many_arguments)]
pub async fn upsert(
    pool: &PgPool,
    var: &str,
    file_name: &str,
    content_type: &str,
    file_size: i64,
    width: i32,
    height: i32,
    blurhash: Option<&str>,
    variants: &[NewVariant],
) -> Result<Vec<String>, DbError> {
    let mut tx = pool.begin().await?;
    let old_files = stale_files(&mut tx, var).await?;
    sqlx::query!("DELETE FROM site_uploads WHERE var = $1", var)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        r#"
        INSERT INTO site_uploads (var, file_name, content_type, file_size, width, height, blurhash)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
        var,
        file_name,
        content_type,
        file_size,
        width,
        height,
        blurhash,
    )
    .execute(&mut *tx)
    .await?;
    if !variants.is_empty() {
        let styles: Vec<&str> = variants.iter().map(|v| v.style.as_str()).collect();
        let file_names: Vec<&str> = variants.iter().map(|v| v.file_name.as_str()).collect();
        let widths: Vec<i32> = variants.iter().map(|v| v.width).collect();
        let heights: Vec<i32> = variants.iter().map(|v| v.height).collect();
        sqlx::query!(
            r#"
            INSERT INTO site_upload_variants (var, style, file_name, width, height)
            SELECT $1, v.style, v.file_name, v.width, v.height
            FROM unnest($2::text[], $3::text[], $4::int[], $5::int[])
                 AS v(style, file_name, width, height)
            "#,
            var,
            &styles as &[&str],
            &file_names as &[&str],
            &widths,
            &heights,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(old_files)
}

/// Updates a slot's alt text (Mastodon's `thumbnail_description` setting).
pub async fn set_description(pool: &PgPool, var: &str, description: &str) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "UPDATE site_uploads SET description = $2, updated_at = now() WHERE var = $1",
        var,
        description,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Empties a slot, returning the storage file names to delete.
pub async fn delete(pool: &PgPool, var: &str) -> Result<Vec<String>, DbError> {
    let mut tx = pool.begin().await?;
    let old_files = stale_files(&mut tx, var).await?;
    sqlx::query!("DELETE FROM site_uploads WHERE var = $1", var)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(old_files)
}

/// Collects the row's original + variant file names inside `tx`.
async fn stale_files(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    var: &str,
) -> Result<Vec<String>, DbError> {
    let mut files = sqlx::query_scalar!("SELECT file_name FROM site_uploads WHERE var = $1", var)
        .fetch_all(&mut **tx)
        .await?;
    files.extend(
        sqlx::query_scalar!(
            "SELECT file_name FROM site_upload_variants WHERE var = $1",
            var
        )
        .fetch_all(&mut **tx)
        .await?,
    );
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn upsert_replaces_slot_and_reports_stale_files(pool: PgPool) {
        assert!(get(&pool, "thumbnail").await.unwrap().is_none());

        let stale = upsert(
            &pool,
            "thumbnail",
            "1.png",
            "image/png",
            10,
            1200,
            630,
            Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH"),
            &[NewVariant {
                style: "@1x".into(),
                file_name: "2.png".into(),
                width: 1200,
                height: 630,
            }],
        )
        .await
        .unwrap();
        assert!(stale.is_empty());

        let row = get(&pool, "thumbnail").await.unwrap().unwrap();
        assert_eq!(row.file_name, "1.png");
        assert_eq!(variants_for(&pool, "thumbnail").await.unwrap().len(), 1);

        assert!(
            set_description(&pool, "thumbnail", "Our island")
                .await
                .unwrap()
        );
        assert_eq!(
            get(&pool, "thumbnail").await.unwrap().unwrap().description,
            "Our island"
        );

        let stale = upsert(
            &pool,
            "thumbnail",
            "3.png",
            "image/png",
            12,
            1200,
            630,
            None,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(stale, vec!["1.png".to_owned(), "2.png".to_owned()]);

        let stale = delete(&pool, "thumbnail").await.unwrap();
        assert_eq!(stale, vec!["3.png".to_owned()]);
        assert!(get(&pool, "thumbnail").await.unwrap().is_none());
        assert!(all(&pool).await.unwrap().is_empty());
    }
}
