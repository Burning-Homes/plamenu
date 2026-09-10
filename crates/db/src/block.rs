//! Account blocks: local blocks (federated out) and inbound remote blocks.

use sqlx::{PgExecutor, PgPool};

use crate::{DbError, id};

/// Records `account_id` blocking `target_account_id`. Idempotent: a repeated
/// block keeps the original row (refreshing the activity URI, for federated
/// re-delivery). Returns the row id, which marks outbound `Block` activities.
/// Executor-generic so the block row commits with its outbox `Block` job (QC
/// audit #18).
pub async fn create<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
    uri: Option<&str>,
) -> Result<i64, DbError> {
    let row_id = sqlx::query_scalar!(
        r#"
        INSERT INTO blocks (id, account_id, target_account_id, uri)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (account_id, target_account_id)
            DO UPDATE SET uri = COALESCE(EXCLUDED.uri, blocks.uri)
        RETURNING id
        "#,
        id::next(),
        account_id,
        target_account_id,
        uri,
    )
    .fetch_one(executor)
    .await?;
    Ok(row_id)
}

/// Removes a block; returns the row id when one existed (it marks the
/// `Undo(Block)` for remote targets). Executor-generic so the removal commits
/// with its outbox `Undo(Block)` job.
pub async fn delete<'e, E: PgExecutor<'e>>(
    executor: E,
    account_id: i64,
    target_account_id: i64,
) -> Result<Option<i64>, DbError> {
    let row_id = sqlx::query_scalar!(
        "DELETE FROM blocks WHERE account_id = $1 AND target_account_id = $2 RETURNING id",
        account_id,
        target_account_id,
    )
    .fetch_optional(executor)
    .await?;
    Ok(row_id)
}

pub async fn exists(
    pool: &PgPool,
    account_id: i64,
    target_account_id: i64,
) -> Result<bool, DbError> {
    let found = sqlx::query_scalar!(
        r#"SELECT 1 AS "one" FROM blocks WHERE account_id = $1 AND target_account_id = $2"#,
        account_id,
        target_account_id,
    )
    .fetch_optional(pool)
    .await?;
    Ok(found.is_some())
}

/// Of `target_ids`, the subset `account_id` blocks (outgoing), in one query.
pub async fn blocking_out_batch(
    pool: &PgPool,
    account_id: i64,
    target_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT target_account_id AS "id!" FROM blocks
           WHERE account_id = $1 AND target_account_id = ANY($2)"#,
        account_id,
        target_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// Of `source_ids`, the subset that block `target_account_id` (incoming), in
/// one query.
pub async fn blocked_by_batch(
    pool: &PgPool,
    target_account_id: i64,
    source_ids: &[i64],
) -> Result<std::collections::HashSet<i64>, DbError> {
    let rows = sqlx::query_scalar!(
        r#"SELECT account_id AS "id!" FROM blocks
           WHERE target_account_id = $1 AND account_id = ANY($2)"#,
        target_account_id,
        source_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// One entry of the `/api/v1/blocks` listing: the block row id (the
/// pagination key) and the blocked account.
#[derive(Debug)]
pub struct BlockListEntry {
    pub row_id: i64,
    pub target_account_id: i64,
}

/// Accounts `account_id` blocks, newest block first, keyset-paginated by
/// block row id like Mastodon's `paginate_by_max_id`.
pub async fn list(
    pool: &PgPool,
    account_id: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    limit: i64,
) -> Result<Vec<BlockListEntry>, DbError> {
    let entries = sqlx::query_as!(
        BlockListEntry,
        r#"
        SELECT id AS "row_id!", target_account_id AS "target_account_id!"
        FROM blocks
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

/// Ids of the *local* accounts that block `target_account_id`. Account
/// migration carries these blocks over to the migration target.
pub async fn local_blocker_ids(pool: &PgPool, target_account_id: i64) -> Result<Vec<i64>, DbError> {
    let ids = sqlx::query_scalar!(
        r#"
        SELECT b.account_id AS "id!"
        FROM blocks b
        JOIN accounts a ON a.id = b.account_id
        WHERE b.target_account_id = $1 AND a.domain IS NULL
        ORDER BY b.account_id
        "#,
        target_account_id,
    )
    .fetch_all(pool)
    .await?;
    Ok(ids)
}

/// Which of `author_ids` are hidden from `viewer` (a block in either
/// direction, an active mute, or a domain block) — the batch form of the SQL
/// `account_hidden` function, for filtering already-fetched threads.
pub async fn hidden_authors(
    pool: &PgPool,
    viewer: i64,
    author_ids: &[i64],
) -> Result<Vec<i64>, DbError> {
    let hidden = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT author.id AS "id!"
        FROM unnest($2::bigint[]) AS author (id)
        WHERE account_hidden($1, author.id)
        "#,
        viewer,
        author_ids,
    )
    .fetch_all(pool)
    .await?;
    Ok(hidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{self, NewLocalAccount};

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
    async fn block_lifecycle_is_idempotent_and_directional(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;

        let first = create(&pool, alice, carol, None).await.unwrap();
        assert!(exists(&pool, alice, carol).await.unwrap());
        assert!(!exists(&pool, carol, alice).await.unwrap(), "directional");

        // Re-blocking keeps the row; a later URI sticks, None does not erase.
        let again = create(&pool, alice, carol, Some("https://x/1"))
            .await
            .unwrap();
        assert_eq!(first, again);
        let again = create(&pool, alice, carol, None).await.unwrap();
        assert_eq!(first, again);

        assert_eq!(delete(&pool, alice, carol).await.unwrap(), Some(first));
        assert!(!exists(&pool, alice, carol).await.unwrap());
        assert_eq!(delete(&pool, alice, carol).await.unwrap(), None);
    }

    #[sqlx::test]
    async fn listing_pages_newest_first(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let carol = local(&pool, "carol").await;
        let dave = local(&pool, "dave").await;
        let first = create(&pool, alice, carol, None).await.unwrap();
        let second = create(&pool, alice, dave, None).await.unwrap();

        let entries = list(&pool, alice, None, None, 10).await.unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.row_id, e.target_account_id))
                .collect::<Vec<_>>(),
            [(second, dave), (first, carol)]
        );
        let older = list(&pool, alice, Some(second), None, 10).await.unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].target_account_id, carol);
        assert!(list(&pool, carol, None, None, 10).await.unwrap().is_empty());
    }

    #[sqlx::test]
    async fn hidden_authors_covers_blocks_mutes_and_domain_blocks(pool: PgPool) {
        let alice = local(&pool, "alice").await;
        let blocked = local(&pool, "blocked").await;
        let blocker = local(&pool, "blocker").await;
        let muted = local(&pool, "muted").await;
        let visible = local(&pool, "visible").await;
        let domain_blocked = crate::account::upsert_remote(
            &pool,
            crate::account::RemoteAccountData {
                username: "remote",
                domain: "blocked.example",
                uri: "https://blocked.example/users/remote",
                display_name: "",
                note: "",
                inbox_url: "https://blocked.example/users/remote/inbox",
                shared_inbox_url: "https://blocked.example/inbox",
                public_key_pem: "pub",
                public_key_id: "https://blocked.example/users/remote#main-key",
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
        .id;

        create(&pool, alice, blocked, None).await.unwrap();
        create(&pool, blocker, alice, None).await.unwrap();
        crate::account_domain_block::create(&pool, alice, "blocked.example")
            .await
            .unwrap();
        crate::mute::upsert(&pool, alice, muted, true, None)
            .await
            .unwrap();
        // An expired mute hides nothing.
        let expired = local(&pool, "expired").await;
        crate::mute::upsert(
            &pool,
            alice,
            expired,
            true,
            Some(time::OffsetDateTime::now_utc() - time::Duration::minutes(1)),
        )
        .await
        .unwrap();

        let all = [
            alice,
            blocked,
            blocker,
            muted,
            visible,
            expired,
            domain_blocked,
        ];
        let mut hidden = hidden_authors(&pool, alice, &all).await.unwrap();
        hidden.sort_unstable();
        let mut expected = vec![blocked, blocker, muted, domain_blocked];
        expected.sort_unstable();
        assert_eq!(hidden, expected);
    }
}
