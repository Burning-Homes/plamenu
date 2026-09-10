//! User-level domain blocks (`account_domain_blocks`): a local account hides a
//! remote domain without changing instance-wide federation policy.

use sqlx::PgPool;

use crate::{DbError, id};

#[derive(Debug)]
pub struct DomainBlockListEntry {
    pub row_id: i64,
    pub domain: String,
}

#[derive(Debug)]
pub struct DomainBlockPreview {
    pub following_count: i64,
    pub followers_count: i64,
}

/// Creates a domain block. Idempotent: a repeated block keeps the original row
/// and returns its id.
pub async fn create<'e, E: sqlx::PgExecutor<'e>>(
    pool: E,
    account_id: i64,
    domain: &str,
) -> Result<i64, DbError> {
    let row_id = sqlx::query_scalar!(
        r#"
        INSERT INTO account_domain_blocks (id, account_id, domain)
        VALUES ($1, $2, $3)
        ON CONFLICT (account_id, domain) DO UPDATE SET domain = EXCLUDED.domain
        RETURNING id
        "#,
        id::next(),
        account_id,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(row_id)
}

/// Removes a domain block. Missing rows are an idempotent no-op.
pub async fn delete(pool: &PgPool, account_id: i64, domain: &str) -> Result<bool, DbError> {
    let result = sqlx::query!(
        "DELETE FROM account_domain_blocks WHERE account_id = $1 AND domain = $2",
        account_id,
        domain,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn exists(pool: &PgPool, account_id: i64, domain: &str) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"
        SELECT 1 AS "one"
        FROM account_domain_blocks
        WHERE account_id = $1 AND domain = $2
        "#,
        account_id,
        domain,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Whether `account_id` has blocked the remote domain of `viewer_id`.
pub async fn blocks_account_domain(
    pool: &PgPool,
    account_id: i64,
    viewer_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"
        SELECT 1 AS "one"
        FROM accounts viewer
        JOIN account_domain_blocks adb
          ON adb.account_id = $1 AND adb.domain = viewer.domain
        WHERE viewer.id = $2 AND NOT viewer.portable
        "#,
        account_id,
        viewer_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Domains `account_id` blocks, newest block first, keyset-paginated by row
/// id like Mastodon's `paginate_by_max_id`.
/// Every domain `account_id` blocks, as a set — fetched once so a page of
/// relationships can test each target's domain locally instead of a per-row
/// [`exists`] query.
pub async fn all_domains(
    pool: &PgPool,
    account_id: i64,
) -> Result<std::collections::HashSet<String>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT domain AS "domain!" FROM account_domain_blocks WHERE account_id = $1"#,
        account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<DomainBlockListEntry>, DbError> {
    let entries = sqlx::query_as!(
        DomainBlockListEntry,
        r#"
        SELECT id AS "row_id!", domain AS "domain!"
        FROM account_domain_blocks
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

/// Counts accepted relationships on the domain for the preview endpoint.
pub async fn preview(
    pool: &PgPool,
    account_id: i64,
    domain: &str,
) -> Result<DomainBlockPreview, DbError> {
    let following_count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM follows f
        JOIN accounts target ON target.id = f.target_account_id
        WHERE f.account_id = $1 AND target.domain = $2 AND NOT f.pending
        "#,
        account_id,
        domain,
    )
    .fetch_one(pool)
    .await?;
    let followers_count = sqlx::query_scalar!(
        r#"
        SELECT count(*) AS "count!"
        FROM follows f
        JOIN accounts follower ON follower.id = f.account_id
        WHERE f.target_account_id = $1 AND follower.domain = $2 AND NOT f.pending
        "#,
        account_id,
        domain,
    )
    .fetch_one(pool)
    .await?;
    Ok(DomainBlockPreview {
        following_count,
        followers_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount, RemoteAccountData};

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

    async fn remote(pool: &PgPool, username: &str, domain: &str) -> i64 {
        account::upsert_remote(
            pool,
            RemoteAccountData {
                username,
                domain,
                uri: &format!("https://{domain}/users/{username}"),
                display_name: "",
                note: "",
                inbox_url: &format!("https://{domain}/users/{username}/inbox"),
                shared_inbox_url: &format!("https://{domain}/inbox"),
                public_key_pem: "pub",
                public_key_id: &format!("https://{domain}/users/{username}#main-key"),
                avatar_remote_url: None,
                header_remote_url: None,
                avatar_description: "",
                header_description: "",
                created_at: None,
                fields: Vec::new(),
                featured_collection_url: None,
                locked: false,
                also_known_as: &[],
                moved_to_uri: None,
                url: None,
                discoverable: false,
                feature_approval_policy: 0,
                is_bot: false,
                indexable: false,
                show_media: None,
                show_media_replies: None,
                show_featured: None,
                memorial: false,
                actor_type: None,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    async fn domain_block_lifecycle_and_listing(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let first = create(&pool, alice, "example.com").await.unwrap();
        let again = create(&pool, alice, "example.com").await.unwrap();
        assert_eq!(first, again);
        assert!(exists(&pool, alice, "example.com").await.unwrap());

        let second = create(&pool, alice, "example.net").await.unwrap();
        let entries = list(&pool, alice, None, None, 10).await.unwrap();
        assert_eq!(
            entries.iter().map(|e| e.row_id).collect::<Vec<_>>(),
            [second, first]
        );

        assert!(delete(&pool, alice, "example.com").await.unwrap());
        assert!(!exists(&pool, alice, "example.com").await.unwrap());
        assert!(!delete(&pool, alice, "example.com").await.unwrap());
    }

    #[sqlx::test]
    async fn preview_counts_accepted_relationships_for_domain(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let bob = remote(&pool, "bob", "remote.example").await;
        let carol = remote(&pool, "carol", "remote.example").await;
        let dave = remote(&pool, "dave", "other.example").await;

        crate::follow::create(&pool, alice, bob, None)
            .await
            .unwrap();
        crate::follow::create_request(&pool, alice, carol, None)
            .await
            .unwrap();
        crate::follow::create(&pool, carol, alice, None)
            .await
            .unwrap();
        crate::follow::create(&pool, dave, alice, None)
            .await
            .unwrap();

        let preview = preview(&pool, alice, "remote.example").await.unwrap();
        assert_eq!(preview.following_count, 1);
        assert_eq!(preview.followers_count, 1);
        let severable = crate::follow::edges_to_domain(&pool, alice, "remote.example")
            .await
            .unwrap();
        assert_eq!(
            severable
                .iter()
                .map(|e| e.other_account_id)
                .collect::<Vec<_>>(),
            [bob, carol]
        );
        let severable = crate::follow::edges_from_domain(&pool, alice, "remote.example")
            .await
            .unwrap();
        assert_eq!(
            severable
                .iter()
                .map(|e| e.other_account_id)
                .collect::<Vec<_>>(),
            [carol]
        );
    }
}
