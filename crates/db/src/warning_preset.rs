//! Canned moderation-strike texts (Mastodon's `AccountWarningPreset`).
//!
//! Presets are picked from a dropdown in the admin account-action form to
//! pre-fill the strike text; they carry no behavior of their own.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WarningPreset {
    pub id: i64,
    pub title: String,
    pub text: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// Every preset, alphabetized by title (Mastodon's
/// `AccountWarningPreset.alphabetic`).
pub async fn list(pool: &PgPool) -> Result<Vec<WarningPreset>, DbError> {
    let presets = sqlx::query_as!(
        WarningPreset,
        r#"
        SELECT id, title, text, created_at, updated_at
        FROM account_warning_presets
        ORDER BY title, text
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(presets)
}

pub async fn find_by_id(pool: &PgPool, preset_id: i64) -> Result<Option<WarningPreset>, DbError> {
    let preset = sqlx::query_as!(
        WarningPreset,
        r#"
        SELECT id, title, text, created_at, updated_at
        FROM account_warning_presets
        WHERE id = $1
        "#,
        preset_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(preset)
}

pub async fn create(pool: &PgPool, title: &str, text: &str) -> Result<WarningPreset, DbError> {
    let preset = sqlx::query_as!(
        WarningPreset,
        r#"
        INSERT INTO account_warning_presets (id, title, text)
        VALUES ($1, $2, $3)
        RETURNING id, title, text, created_at, updated_at
        "#,
        id::next(),
        title,
        text,
    )
    .fetch_one(pool)
    .await?;
    Ok(preset)
}

/// Rewrites a preset's title and text. Returns `None` for an unknown id.
pub async fn update(
    pool: &PgPool,
    preset_id: i64,
    title: &str,
    text: &str,
) -> Result<Option<WarningPreset>, DbError> {
    let preset = sqlx::query_as!(
        WarningPreset,
        r#"
        UPDATE account_warning_presets SET
            title      = $2,
            text       = $3,
            updated_at = now()
        WHERE id = $1
        RETURNING id, title, text, created_at, updated_at
        "#,
        preset_id,
        title,
        text,
    )
    .fetch_optional(pool)
    .await?;
    Ok(preset)
}

/// Deletes a preset. Returns `false` when no row matched.
pub async fn delete(pool: &PgPool, preset_id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query!(
        r#"DELETE FROM account_warning_presets WHERE id = $1"#,
        preset_id,
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok(affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn crud_round_trip_alphabetized(pool: PgPool) {
        let spam = create(&pool, "Spam", "Please stop posting spam.")
            .await
            .unwrap();
        create(&pool, "Conduct", "Please review our rules.")
            .await
            .unwrap();

        let titles: Vec<_> = list(&pool)
            .await
            .unwrap()
            .iter()
            .map(|p| p.title.clone())
            .collect();
        assert_eq!(titles, ["Conduct", "Spam"]);

        let updated = update(&pool, spam.id, "Spam (final)", "Last warning.")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.text, "Last warning.");
        assert_eq!(
            find_by_id(&pool, spam.id).await.unwrap().unwrap().title,
            "Spam (final)"
        );

        assert!(delete(&pool, spam.id).await.unwrap());
        assert!(!delete(&pool, spam.id).await.unwrap());
        assert!(find_by_id(&pool, spam.id).await.unwrap().is_none());
    }
}
