//! Versioned Terms of Service documents (Mastodon's `TermsOfService`).
//!
//! At most one draft (`published_at IS NULL`) exists at a time; the admin
//! editor rewrites it in place and publishing stamps `published_at`. The
//! *current* document is the newest published one already in effect
//! (`effective_date` NULL or not in the future), falling back to the earliest
//! upcoming one when nothing is effective yet — Mastodon's
//! `TermsOfService.current`.

use sqlx::PgPool;
use time::{Date, OffsetDateTime};

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TermsOfService {
    pub id: i64,
    pub text: String,
    pub changelog: String,
    pub published_at: Option<OffsetDateTime>,
    pub effective_date: Option<Date>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl TermsOfService {
    /// Published and already in effect (Mastodon's `effective?`).
    #[must_use]
    pub fn effective(&self) -> bool {
        self.published_at.is_some()
            && self
                .effective_date
                .is_none_or(|date| date <= OffsetDateTime::now_utc().date())
    }
}

/// The single draft, if one exists.
pub async fn draft(pool: &PgPool) -> Result<Option<TermsOfService>, DbError> {
    let tos = sqlx::query_as!(
        TermsOfService,
        r#"
        SELECT id, text, changelog, published_at, effective_date, created_at, updated_at
        FROM terms_of_services
        WHERE published_at IS NULL
        ORDER BY id DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await?;
    Ok(tos)
}

/// The document currently in force: the newest effective one, else the
/// earliest upcoming one (published but not yet effective).
pub async fn current(pool: &PgPool) -> Result<Option<TermsOfService>, DbError> {
    let tos = sqlx::query_as!(
        TermsOfService,
        r#"
        SELECT id, text, changelog, published_at, effective_date, created_at, updated_at
        FROM terms_of_services
        WHERE published_at IS NOT NULL
        ORDER BY (effective_date IS NULL OR effective_date <= CURRENT_DATE) DESC,
                 CASE WHEN effective_date IS NULL OR effective_date <= CURRENT_DATE
                      THEN COALESCE(effective_date, published_at::date) END DESC,
                 effective_date ASC,
                 id DESC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await?;
    Ok(tos)
}

/// Every published version, newest first.
pub async fn list_published(pool: &PgPool) -> Result<Vec<TermsOfService>, DbError> {
    let versions = sqlx::query_as!(
        TermsOfService,
        r#"
        SELECT id, text, changelog, published_at, effective_date, created_at, updated_at
        FROM terms_of_services
        WHERE published_at IS NOT NULL
        ORDER BY COALESCE(effective_date, published_at::date) DESC, id DESC
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(versions)
}

/// The effective date of the published version that supersedes `tos_id`
/// (Mastodon's `succeeded_by`): the next `effective_date` at or after `after`.
pub async fn next_effective_after(
    pool: &PgPool,
    tos_id: i64,
    after: Option<Date>,
) -> Result<Option<Date>, DbError> {
    let date = sqlx::query_scalar!(
        r#"
        SELECT effective_date AS "effective_date!"
        FROM terms_of_services
        WHERE published_at IS NOT NULL
          AND id <> $1
          AND effective_date IS NOT NULL
          AND ($2::date IS NULL OR effective_date >= $2)
        ORDER BY effective_date
        LIMIT 1
        "#,
        tos_id,
        after,
    )
    .fetch_optional(pool)
    .await?;
    Ok(date)
}

/// Creates or rewrites the single draft.
pub async fn save_draft(
    pool: &PgPool,
    text: &str,
    changelog: &str,
    effective_date: Option<Date>,
) -> Result<TermsOfService, DbError> {
    if let Some(existing) = draft(pool).await? {
        let tos = sqlx::query_as!(
            TermsOfService,
            r#"
            UPDATE terms_of_services SET
                text           = $2,
                changelog      = $3,
                effective_date = $4,
                updated_at     = now()
            WHERE id = $1
            RETURNING id, text, changelog, published_at, effective_date, created_at, updated_at
            "#,
            existing.id,
            text,
            changelog,
            effective_date,
        )
        .fetch_one(pool)
        .await?;
        return Ok(tos);
    }
    let tos = sqlx::query_as!(
        TermsOfService,
        r#"
        INSERT INTO terms_of_services (id, text, changelog, effective_date)
        VALUES ($1, $2, $3, $4)
        RETURNING id, text, changelog, published_at, effective_date, created_at, updated_at
        "#,
        id::next(),
        text,
        changelog,
        effective_date,
    )
    .fetch_one(pool)
    .await?;
    Ok(tos)
}

/// Publishes the draft. Returns `None` when the id is unknown or already
/// published.
pub async fn publish(pool: &PgPool, tos_id: i64) -> Result<Option<TermsOfService>, DbError> {
    let tos = sqlx::query_as!(
        TermsOfService,
        r#"
        UPDATE terms_of_services SET
            published_at = now(),
            updated_at   = now()
        WHERE id = $1 AND published_at IS NULL
        RETURNING id, text, changelog, published_at, effective_date, created_at, updated_at
        "#,
        tos_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(tos)
}

#[cfg(test)]
mod tests {
    use time::macros::date;

    use super::*;

    #[sqlx::test]
    async fn draft_is_singular_and_publishes(pool: PgPool) {
        assert!(draft(&pool).await.unwrap().is_none());
        assert!(current(&pool).await.unwrap().is_none());

        let first = save_draft(&pool, "Be kind.", "", None).await.unwrap();
        let second = save_draft(&pool, "Be kinder.", "Tone", None).await.unwrap();
        assert_eq!(first.id, second.id, "draft is rewritten in place");

        let published = publish(&pool, second.id).await.unwrap().unwrap();
        assert!(published.published_at.is_some());
        assert!(
            published.effective(),
            "no effective date = live immediately"
        );
        assert!(publish(&pool, second.id).await.unwrap().is_none());

        assert!(draft(&pool).await.unwrap().is_none());
        assert_eq!(current(&pool).await.unwrap().unwrap().id, published.id);
    }

    #[sqlx::test]
    async fn current_prefers_effective_over_upcoming(pool: PgPool) {
        let live = save_draft(&pool, "v1", "", Some(date!(2020 - 01 - 01)))
            .await
            .unwrap();
        publish(&pool, live.id).await.unwrap();
        let upcoming = save_draft(&pool, "v2", "Update", Some(date!(2999 - 01 - 01)))
            .await
            .unwrap();
        publish(&pool, upcoming.id).await.unwrap();

        // The effective one wins over the future one…
        let now = current(&pool).await.unwrap().unwrap();
        assert_eq!(now.id, live.id);
        assert!(now.effective());

        // …and it is succeeded by the upcoming version's date.
        assert_eq!(
            next_effective_after(&pool, now.id, now.effective_date)
                .await
                .unwrap(),
            Some(date!(2999 - 01 - 01))
        );

        let versions = list_published(&pool).await.unwrap();
        let texts: Vec<_> = versions.iter().map(|v| v.text.as_str()).collect();
        assert_eq!(texts, ["v2", "v1"]);
    }

    #[sqlx::test]
    async fn upcoming_only_still_serves_as_current(pool: PgPool) {
        let upcoming = save_draft(&pool, "soon", "", Some(date!(2999 - 06 - 01)))
            .await
            .unwrap();
        publish(&pool, upcoming.id).await.unwrap();

        let now = current(&pool).await.unwrap().unwrap();
        assert_eq!(now.id, upcoming.id);
        assert!(!now.effective());
    }
}
