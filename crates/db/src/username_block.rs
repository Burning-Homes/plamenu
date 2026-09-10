//! Sign-up username reservations (Mastodon's `UsernameBlock`).
//!
//! Matching runs against a normalized form: lowercased with the digits
//! commonly used as letter stand-ins folded to the letters they imitate
//! (Mastodon's `HOMOGLYPHS` map), so reserving `admin` also stops `4dm1n`.
//! `exact` rows must equal the whole normalized username; the rest match as
//! substrings. `allow_with_approval` rows don't reject the sign-up — they
//! force it into the manual-approval queue instead.

use sqlx::PgPool;
use time::OffsetDateTime;

use crate::{DbError, id};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsernameBlock {
    pub id: i64,
    pub username: String,
    pub normalized_username: String,
    pub exact: bool,
    pub allow_with_approval: bool,
    pub created_at: OffsetDateTime,
}

/// Lowercases and folds digit homoglyphs (`1`→`i`, `3`→`e`, `0`→`o`, …),
/// mirroring Mastodon's `UsernameBlock` normalization.
#[must_use]
pub fn normalize(username: &str) -> String {
    username
        .to_lowercase()
        .chars()
        .map(|c| match c {
            '1' => 'i',
            '2' => 'z',
            '3' => 'e',
            '4' => 'a',
            '5' => 's',
            '7' => 't',
            '8' => 'b',
            '9' => 'g',
            '0' => 'o',
            other => other,
        })
        .collect()
}

/// Every block, alphabetized by the username as entered.
pub async fn list(pool: &PgPool) -> Result<Vec<UsernameBlock>, DbError> {
    let blocks = sqlx::query_as!(
        UsernameBlock,
        r#"
        SELECT id, username, normalized_username, exact, allow_with_approval, created_at
        FROM username_blocks
        ORDER BY username
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(blocks)
}

pub async fn find_by_id(pool: &PgPool, block_id: i64) -> Result<Option<UsernameBlock>, DbError> {
    let block = sqlx::query_as!(
        UsernameBlock,
        r#"
        SELECT id, username, normalized_username, exact, allow_with_approval, created_at
        FROM username_blocks
        WHERE id = $1
        "#,
        block_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(block)
}

/// Whether `username` hits any block with the given `allow_with_approval`
/// mode — Mastodon's `UsernameBlock.matches?`. Callers check the two modes
/// separately: `false` blocks the sign-up outright, `true` only forces the
/// approval queue.
pub async fn matches(
    pool: &PgPool,
    username: &str,
    allow_with_approval: bool,
) -> Result<bool, DbError> {
    let normalized = normalize(username);
    let hit = sqlx::query_scalar!(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM username_blocks
            WHERE allow_with_approval = $2
              AND ((exact AND normalized_username = $1)
                   OR (NOT exact AND $1 LIKE '%' || normalized_username || '%'))
        ) AS "hit!"
        "#,
        normalized,
        allow_with_approval,
    )
    .fetch_one(pool)
    .await?;
    Ok(hit)
}

pub async fn create(
    pool: &PgPool,
    username: &str,
    exact: bool,
    allow_with_approval: bool,
) -> Result<UsernameBlock, DbError> {
    let block = sqlx::query_as!(
        UsernameBlock,
        r#"
        INSERT INTO username_blocks (id, username, normalized_username, exact, allow_with_approval)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING id, username, normalized_username, exact, allow_with_approval, created_at
        "#,
        id::next(),
        username,
        normalize(username),
        exact,
        allow_with_approval,
    )
    .fetch_one(pool)
    .await?;
    Ok(block)
}

/// Rewrites a block in place (re-deriving the normalized form). Returns
/// `None` for an unknown id.
pub async fn update(
    pool: &PgPool,
    block_id: i64,
    username: &str,
    exact: bool,
    allow_with_approval: bool,
) -> Result<Option<UsernameBlock>, DbError> {
    let block = sqlx::query_as!(
        UsernameBlock,
        r#"
        UPDATE username_blocks SET
            username            = $2,
            normalized_username = $3,
            exact               = $4,
            allow_with_approval = $5
        WHERE id = $1
        RETURNING id, username, normalized_username, exact, allow_with_approval, created_at
        "#,
        block_id,
        username,
        normalize(username),
        exact,
        allow_with_approval,
    )
    .fetch_optional(pool)
    .await?;
    Ok(block)
}

/// Deletes a block. Returns `false` when no row matched.
pub async fn delete(pool: &PgPool, block_id: i64) -> Result<bool, DbError> {
    let affected = sqlx::query!(r#"DELETE FROM username_blocks WHERE id = $1"#, block_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(affected > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_case_and_homoglyphs() {
        assert_eq!(normalize("Admin"), "admin");
        assert_eq!(normalize("4dm1n"), "admin");
        assert_eq!(normalize("m0d3rat0r"), "moderator");
        assert_eq!(normalize("plain_name"), "plain_name");
    }

    #[sqlx::test]
    async fn exact_and_partial_matching(pool: PgPool) {
        create(&pool, "admin", true, false).await.unwrap();
        create(&pool, "staff", false, false).await.unwrap();

        // Exact: homoglyph variants of the whole name hit, supersets don't.
        assert!(matches(&pool, "4dm1n", false).await.unwrap());
        assert!(!matches(&pool, "administrator", false).await.unwrap());

        // Partial: any username containing the normalized needle hits.
        assert!(matches(&pool, "the_5taff_desk", false).await.unwrap());
        assert!(!matches(&pool, "harmless", false).await.unwrap());
    }

    #[sqlx::test]
    async fn approval_mode_matches_separately(pool: PgPool) {
        create(&pool, "press", true, true).await.unwrap();

        assert!(!matches(&pool, "press", false).await.unwrap());
        assert!(matches(&pool, "press", true).await.unwrap());
    }

    #[sqlx::test]
    async fn update_and_delete(pool: PgPool) {
        let block = create(&pool, "root", true, false).await.unwrap();
        let updated = update(&pool, block.id, "R00T", false, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.normalized_username, "root");
        assert!(!updated.exact);
        assert!(updated.allow_with_approval);

        assert!(delete(&pool, block.id).await.unwrap());
        assert!(!delete(&pool, block.id).await.unwrap());
        assert!(list(&pool).await.unwrap().is_empty());
    }
}
