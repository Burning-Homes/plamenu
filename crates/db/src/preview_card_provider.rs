//! Link publishers — Mastodon's `preview_card_providers`. A domain-level review
//! gate: a preview card with no explicit `trendable` inherits its provider's,
//! and a card "requires review" until its provider (or the card itself) is
//! reviewed. Providers are created lazily when a card's domain is first seen by
//! the scorer.

use std::collections::HashSet;

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

/// A link publisher's review row.
#[derive(Debug, Clone)]
pub struct Provider {
    pub id: i64,
    pub domain: String,
    pub trendable: Option<bool>,
    pub reviewed_at: Option<OffsetDateTime>,
    pub requested_review_at: Option<OffsetDateTime>,
}

impl Provider {
    /// Still awaits moderator review, matching Mastodon's reviewable
    /// semantics — never reviewed.
    #[must_use]
    pub fn requires_review(&self) -> bool {
        self.reviewed_at.is_none()
    }
}

/// The set of domains whose provider is explicitly trendable — used to resolve a
/// card's `allowed` flag when the card has no override of its own.
pub async fn trendable_domains(pool: &PgPool) -> Result<HashSet<String>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT domain AS "domain!" FROM preview_card_providers WHERE trendable IS TRUE"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// The set of domains with a review decision recorded (`reviewed_at` set) — a
/// card whose provider is in this set no longer `requires_review`.
pub async fn reviewed_domains(pool: &PgPool) -> Result<HashSet<String>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT domain AS "domain!" FROM preview_card_providers WHERE reviewed_at IS NOT NULL"#,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Finds or creates the provider row for a domain (idempotent).
pub async fn ensure(pool: &PgPool, domain: &str) -> Result<i64, DbError> {
    let provider_id = sqlx::query_scalar!(
        r#"
        INSERT INTO preview_card_providers (id, domain)
        VALUES ($1, $2)
        ON CONFLICT (domain) DO UPDATE SET domain = preview_card_providers.domain
        RETURNING id
        "#,
        id::next(),
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(provider_id)
}

/// Batched [`ensure`]: creates any missing provider rows for the whole set of
/// domains in one statement. Callers that need the ids still use [`ensure`];
/// the scorer only needs the rows to exist.
pub async fn ensure_many(pool: &PgPool, domains: &[String]) -> Result<(), DbError> {
    if domains.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = domains.iter().map(|_| id::next()).collect();
    sqlx::query!(
        r#"
        INSERT INTO preview_card_providers (id, domain)
        SELECT * FROM unnest($1::bigint[], $2::text[])
        ON CONFLICT (domain) DO NOTHING
        "#,
        &ids,
        domains,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// All providers, newest id first, keyset-paginated by id (the admin listing).
pub async fn list(
    pool: &PgPool,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Provider>, DbError> {
    if let Some(min_id) = min_id {
        let mut rows = sqlx::query_as!(
            Provider,
            r#"
            SELECT id, domain, trendable, reviewed_at, requested_review_at
            FROM preview_card_providers
            WHERE id > $1 AND ($2::bigint IS NULL OR id < $2)
            ORDER BY id ASC
            LIMIT $3
            "#,
            min_id,
            max_id,
            limit,
        )
        .fetch_all(pool)
        .await?;
        rows.reverse();
        return Ok(rows);
    }
    let rows = sqlx::query_as!(
        Provider,
        r#"
        SELECT id, domain, trendable, reviewed_at, requested_review_at
        FROM preview_card_providers
        WHERE ($1::bigint IS NULL OR id < $1) AND ($2::bigint IS NULL OR id > $2)
        ORDER BY id DESC
        LIMIT $3
        "#,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Sets a provider's `trendable` and stamps `reviewed_at` (approve/reject).
/// Returns the updated row, or `None` if the id is unknown.
pub async fn set_trendable(
    pool: &PgPool,
    id: i64,
    trendable: bool,
    reviewed_at: OffsetDateTime,
) -> Result<Option<Provider>, DbError> {
    let row = sqlx::query_as!(
        Provider,
        r#"
        UPDATE preview_card_providers
        SET trendable = $2, reviewed_at = $3, updated_at = now()
        WHERE id = $1
        RETURNING id, domain, trendable, reviewed_at, requested_review_at
        "#,
        id,
        trendable,
        reviewed_at,
    )
    .fetch_optional(pool)
    .await?;
    Ok(row)
}
